//! Library view module -- browse, search, and download assets from an Immich server.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use glib::clone;
use gtk::prelude::*;
use libadwaita::prelude::*;

use crate::api_client::{LibraryAsset, MetadataSearchFilters};
use crate::app_context::AppContext;
use crate::library::albums_view::{
    AlbumClick, AlbumsViewParts, build_albums_view, populate_albums,
};
use crate::library::explore_view::{ExploreViewParts, build_explore_view};
use crate::library::grid_view::{GridViewParts, build_grid_view};
use crate::library::local_source::{
    LocalAsset, enumerate_local, enumerate_local_for_entry, filter_by_filename,
};
use crate::library::sidebar::{SidebarParts, build_sidebar};
use crate::library::state::{LibraryLoadState, LibrarySource};
use crate::state_manager::TransferDirection;

use self::actions::{connect_bulk_actions, connect_select_mode};
use self::album_link::{connect_album_link_row, refresh_album_link_row};
use self::context_menu::show_asset_context_menu;
use self::controls::{
    connect_controls, connect_grid_handlers, connect_sidebar_handlers,
    refresh_library_after_mutation, sidebar_dispatch,
};
use self::download::format_rate;
use self::filters::connect_filters_button;
use self::lightbox::open_lightbox;

pub(super) const LOCAL_ID_PREFIX: &str = "local::";

pub mod album_sync;
pub mod albums_view;
pub mod asset_model;
pub mod asset_object;
pub mod explore_view;
pub mod grid_view;
pub mod local_source;
pub mod sidebar;
pub mod state;
pub mod style;
pub mod thumbnail_cache;

mod actions;
mod album_link;
mod context_menu;
mod controls;
mod download;
mod filters;
mod lightbox;

const PAGE_SIZE: u32 = 50;

fn register_app_icons() {
    if let Some(display) = gtk::gdk::Display::default() {
        let theme = gtk::IconTheme::for_display(&display);
        theme.add_resource_path("/dev/nicx/mimick/icons");
    }
}

fn load_texture_oriented(path: &std::path::Path) -> Option<gdk4::Texture> {
    let raw = gtk::gdk_pixbuf::Pixbuf::from_file(path).ok()?;
    let pixbuf = raw.apply_embedded_orientation().unwrap_or(raw);
    #[allow(deprecated)]
    Some(gdk4::Texture::for_pixbuf(&pixbuf))
}

struct LibraryWindowUi {
    ctx: Arc<AppContext>,
    app: libadwaita::Application,
    window: libadwaita::ApplicationWindow,
    nav: libadwaita::NavigationView,
    sidebar: SidebarParts,
    grid: GridViewParts,
    explore: ExploreViewParts,
    albums: AlbumsViewParts,
    content_stack: gtk::Stack,
    error_label: gtk::Label,
    transfer_bar: gtk::Box,
    transfer_progress: gtk::ProgressBar,
    transfer_icon: gtk::Image,
    transfer_label: gtk::Label,
    search_entry: gtk::SearchEntry,
    search_mode: gtk::DropDown,
    sort_mode: gtk::DropDown,
    source_mode: gtk::DropDown,
    filters_button: gtk::Button,
    timeline_toggle: gtk::ToggleButton,
    timeline_banner: gtk::Label,
    source_mode_suppressed: Cell<bool>,
    sidebar_suppressed: Cell<bool>,
    back_button: gtk::Button,
    select_toggle: gtk::ToggleButton,
    bulk_bar: gtk::Revealer,
    bulk_count_label: gtk::Label,
    album_link_row: libadwaita::ActionRow,
    album_link_button: gtk::Button,
    album_sync_button: gtk::Button,
    last_seen_upload_batch: Cell<u64>,
    narrow: Rc<Cell<bool>>,
    split: libadwaita::OverlaySplitView,
}

pub fn build_library_window(app: &libadwaita::Application, ctx: Arc<AppContext>) {
    style::ensure_registered();
    register_app_icons();

    let window = libadwaita::ApplicationWindow::builder()
        .application(app)
        .title("Mimick Library")
        .name("mimick-library-window")
        .default_width(1180)
        .default_height(780)
        .width_request(360)
        .height_request(480)
        .build();

    let header = libadwaita::HeaderBar::builder()
        .show_start_title_buttons(true)
        .show_end_title_buttons(true)
        .build();
    let sidebar_toggle = gtk::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Toggle sidebar (F9)")
        .active(true)
        .build();
    let back_button = gtk::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Back (Alt+Left)")
        .sensitive(false)
        .build();
    let prefs_button = gtk::Button::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Open Settings")
        .build();
    let queue_button = gtk::Button::builder()
        .icon_name("view-list-symbolic")
        .tooltip_text("Open Queue Inspector")
        .build();
    let refresh_button = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh")
        .build();
    header.pack_start(&sidebar_toggle);
    header.pack_start(&back_button);
    header.pack_end(&prefs_button);
    header.pack_end(&queue_button);
    header.pack_end(&refresh_button);
    let select_toggle = gtk::ToggleButton::builder()
        .icon_name("checkbox-symbolic")
        .tooltip_text("Select assets (Esc to exit)")
        .build();

    let toolbar = libadwaita::ToolbarView::builder().build();
    toolbar.add_top_bar(&header);

    let narrow_flag = Rc::new(Cell::new(false));
    let sidebar = build_sidebar();
    let grid = build_grid_view(ctx.clone(), select_toggle.clone(), narrow_flag.clone());
    let explore = build_explore_view();
    let albums = build_albums_view();

    let source_mode_model = gtk::StringList::new(&["Remote", "Local", "Unified"]);
    let source_mode = gtk::DropDown::builder()
        .model(&source_mode_model)
        .selected(0)
        .tooltip_text("Asset source")
        .hexpand(true)
        .build();
    let timeline_toggle = gtk::ToggleButton::builder()
        .label("Timeline")
        .tooltip_text("Timeline view (all assets only)")
        .build();

    // Three distinct search dimensions, each routed to a different Immich
    // endpoint shape. Smart and OCR are *separate* fields on the Immich
    // search DTOs (`query` vs `ocr` per the live OpenAPI spec), so we
    // expose them independently rather than collapsing OCR into Smart.
    let search_mode_model = gtk::StringList::new(&["Filename", "Smart Search", "OCR"]);
    let search_mode = gtk::DropDown::builder()
        .model(&search_mode_model)
        .selected(0)
        .tooltip_text(
            "Filename: matches the file name and EXIF metadata.\n\
             Smart: CLIP-based semantic search — natural-language queries against visual scenes \
             (\"sunset beach\", \"birthday cake\", \"invoices\").\n\
             OCR: matches text recognised inside images by Immich's ML pipeline. Faster than \
             Smart since it skips CLIP inference.",
        )
        .build();
    let search_entry = gtk::SearchEntry::builder()
        .placeholder_text("Search filenames")
        .hexpand(true)
        .width_chars(20)
        .max_width_chars(24)
        .build();
    let filters_button = gtk::Button::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Advanced filters (date, location, camera, EXIF)")
        .build();
    let sort_model = gtk::StringList::new(&["Newest", "Filename", "File Type"]);
    let sort_mode = gtk::DropDown::builder()
        .model(&sort_model)
        .selected(0)
        .hexpand(true)
        .build();

    let source_group = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    source_group.append(&source_mode);
    source_group.append(&timeline_toggle);

    let search_group = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .hexpand(true)
        .build();
    search_group.append(&search_mode);
    search_group.append(&search_entry);
    search_group.append(&filters_button);

    let sort_group = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    sort_group.append(&sort_mode);

    let controls = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    controls.append(&source_group);
    controls.append(&search_group);
    controls.append(&sort_group);

    let timeline_banner = gtk::Label::builder()
        .xalign(0.0)
        .css_classes(vec!["mimick-timeline-banner".to_string()])
        .visible(false)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(20)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(12)
        .build();

    let content_stack = gtk::Stack::builder().vexpand(true).hexpand(true).build();
    let loading_view = build_status_view(
        "view-refresh-symbolic",
        "Loading…",
        "Fetching library data from the Immich server",
    );
    let empty_view = build_status_view(
        "image-x-generic-symbolic",
        "Nothing to show",
        "No assets match the current view",
    );
    let error_view = build_status_view(
        "dialog-warning-symbolic",
        "Library data unavailable",
        "Could not load library assets",
    );
    let error_label = error_view
        .last_child()
        .and_downcast::<gtk::Label>()
        .expect("status-view subtitle label");
    content_stack.add_named(&loading_view, Some("loading"));
    content_stack.add_named(&empty_view, Some("empty"));
    content_stack.add_named(&error_view, Some("error"));
    content_stack.add_named(&grid.scrolled, Some("grid"));
    content_stack.add_named(&explore.root, Some("explore"));
    content_stack.add_named(&albums.root, Some("albums"));

    let transfer_progress = gtk::ProgressBar::builder()
        .hexpand(true)
        .valign(gtk::Align::Center)
        .css_classes(vec!["mimick-transfer-progress".to_string()])
        .build();
    let transfer_icon = gtk::Image::builder()
        .icon_size(gtk::IconSize::Normal)
        .css_classes(vec!["dim-label".to_string()])
        .visible(false)
        .build();
    let transfer_label = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .wrap(true)
        .max_width_chars(24)
        .width_chars(12)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
        .build();
    let transfer_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(8)
        .margin_bottom(16)
        .margin_start(12)
        .margin_end(12)
        .css_classes(vec!["mimick-transfer-shell".to_string()])
        .build();
    transfer_bar.append(&transfer_progress);
    transfer_bar.append(&transfer_icon);
    transfer_bar.append(&transfer_label);

    let album_link_row = libadwaita::ActionRow::builder()
        .title("No local folder linked")
        .subtitle("Drop files in the linked folder to sync this album")
        .title_lines(1)
        .subtitle_lines(2)
        .build();
    let album_sync_button = gtk::Button::builder()
        .label("Sync…")
        .valign(gtk::Align::Center)
        .css_classes(vec!["suggested-action".to_string()])
        .visible(false)
        .build();
    let album_link_button = gtk::Button::builder()
        .label("Link folder…")
        .valign(gtk::Align::Center)
        .build();
    album_link_row.add_suffix(&album_sync_button);
    album_link_row.add_suffix(&album_link_button);
    let album_link_listbox = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(vec!["boxed-list".to_string()])
        .margin_start(12)
        .margin_end(12)
        .margin_top(4)
        .margin_bottom(4)
        .visible(false)
        .build();
    album_link_listbox.append(&album_link_row);

    let bulk_count_label = gtk::Label::builder().xalign(0.0).hexpand(true).build();
    let bulk_delete = gtk::Button::builder()
        .label("Delete")
        .css_classes(vec!["destructive-action".to_string()])
        .build();
    let bulk_download = gtk::Button::builder().label("Download").build();
    let bulk_clear = gtk::Button::builder()
        .label("Clear")
        .css_classes(vec!["flat".to_string()])
        .build();
    let bulk_inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(8)
        .margin_bottom(16)
        .margin_start(12)
        .margin_end(12)
        .css_classes(vec!["toolbar".to_string()])
        .build();
    bulk_inner.append(&bulk_count_label);
    bulk_inner.append(&bulk_clear);
    bulk_inner.append(&bulk_download);
    bulk_inner.append(&bulk_delete);
    let bulk_bar = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideUp)
        .reveal_child(false)
        .child(&bulk_inner)
        .build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    content.append(&controls);
    content.append(&album_link_listbox);
    content.append(&timeline_banner);
    content.append(&content_stack);
    content.append(&bulk_bar);
    content.append(&transfer_bar);

    let split = libadwaita::OverlaySplitView::builder()
        .sidebar(&sidebar.root)
        .content(&content)
        .show_sidebar(true)
        .enable_show_gesture(true)
        .enable_hide_gesture(true)
        .build();
    split
        .bind_property("show-sidebar", &sidebar_toggle, "active")
        .sync_create()
        .bidirectional()
        .build();
    toolbar.set_content(Some(&split));

    let nav = libadwaita::NavigationView::new();
    let root_page = libadwaita::NavigationPage::builder()
        .child(&toolbar)
        .title("Library")
        .can_pop(false)
        .build();
    nav.add(&root_page);
    window.set_content(Some(&nav));

    let breakpoint = libadwaita::Breakpoint::new(
        libadwaita::BreakpointCondition::parse("max-width: 600sp")
            .expect("valid breakpoint condition"),
    );
    breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
    breakpoint.add_setter(
        &controls,
        "orientation",
        Some(&gtk::Orientation::Vertical.to_value()),
    );
    breakpoint.add_setter(&grid.view, "max-columns", Some(&2u32.to_value()));
    breakpoint.add_setter(&grid.view, "min-columns", Some(&2u32.to_value()));
    breakpoint.add_setter(&refresh_button, "visible", Some(&false.to_value()));
    breakpoint.add_setter(&queue_button, "visible", Some(&false.to_value()));
    breakpoint.add_setter(&bulk_delete, "label", Some(&"Del".to_value()));
    breakpoint.add_setter(&bulk_download, "label", Some(&"Get".to_value()));
    breakpoint.add_setter(&bulk_clear, "label", Some(&"X".to_value()));
    breakpoint.add_setter(&album_sync_button, "label", Some(&"Sync".to_value()));
    breakpoint.add_setter(&album_link_button, "label", Some(&"Link".to_value()));
    breakpoint.add_setter(&transfer_bar, "visible", Some(&false.to_value()));
    let nav_for_apply = nav.clone();
    let narrow_apply = narrow_flag.clone();
    breakpoint.connect_apply(move |_| {
        narrow_apply.set(true);
        apply_narrow_recursive(nav_for_apply.upcast_ref::<gtk::Widget>(), true);
    });
    let nav_for_unapply = nav.clone();
    let narrow_unapply = narrow_flag.clone();
    breakpoint.connect_unapply(move |_| {
        narrow_unapply.set(false);
        apply_narrow_recursive(nav_for_unapply.upcast_ref::<gtk::Widget>(), false);
    });
    window.add_breakpoint(breakpoint);

    // Tablet-width breakpoint: collapse sidebar to overlay before the inline
    // sidebar + controls (~960 px natural) overflow a shrunk desktop window.
    let tablet_bp = libadwaita::Breakpoint::new(
        libadwaita::BreakpointCondition::parse("max-width: 1000sp")
            .expect("valid breakpoint condition"),
    );
    tablet_bp.add_setter(&split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(tablet_bp);

    let f9 = gtk::Shortcut::builder()
        .trigger(&gtk::ShortcutTrigger::parse_string("F9").unwrap())
        .action(&gtk::CallbackAction::new(clone!(
            #[strong]
            split,
            move |_, _| {
                split.set_show_sidebar(!split.shows_sidebar());
                glib::Propagation::Stop
            }
        )))
        .build();
    let shortcut_controller = gtk::ShortcutController::new();
    shortcut_controller.add_shortcut(f9);
    window.add_controller(shortcut_controller);

    let ui = Rc::new(LibraryWindowUi {
        ctx,
        app: app.clone(),
        window: window.clone(),
        nav: nav.clone(),
        sidebar,
        grid,
        explore,
        albums,
        content_stack,
        error_label,
        transfer_bar,
        transfer_progress,
        transfer_icon,
        transfer_label,
        search_entry,
        search_mode,
        sort_mode,
        source_mode,
        filters_button: filters_button.clone(),
        timeline_toggle,
        timeline_banner,
        source_mode_suppressed: Cell::new(false),
        sidebar_suppressed: Cell::new(false),
        back_button: back_button.clone(),
        select_toggle: select_toggle.clone(),
        bulk_bar: bulk_bar.clone(),
        bulk_count_label: bulk_count_label.clone(),
        album_link_row: album_link_row.clone(),
        album_link_button: album_link_button.clone(),
        album_sync_button: album_sync_button.clone(),
        last_seen_upload_batch: Cell::new(0),
        narrow: narrow_flag.clone(),
        split: split.clone(),
    });
    *ui.grid.context_menu_handler.borrow_mut() = Some(Box::new(clone!(
        #[strong]
        ui,
        move |position, x, y| {
            show_asset_context_menu(ui.clone(), &ui.grid.view, position, x, y);
        }
    )));

    connect_album_link_row(ui.clone(), album_link_listbox);

    connect_select_mode(ui.clone(), select_toggle.clone());
    connect_bulk_actions(ui.clone(), bulk_delete, bulk_download, bulk_clear);

    connect_sidebar_handlers(ui.clone());
    connect_controls(ui.clone(), prefs_button, queue_button, refresh_button);
    connect_grid_handlers(ui.clone());
    connect_filters_button(ui.clone(), filters_button);

    bootstrap_window(ui);
    window.present();
}

fn bootstrap_window(ui: Rc<LibraryWindowUi>) {
    let initial_request = {
        let mut state = ui.ctx.library_state.lock();
        state.load_initial_source()
    };

    apply_timeline_ui_state(&ui, &initial_request.1);
    load_albums(ui.clone());
    load_status(ui.clone());
    fetch_current_user(ui.clone());
    connect_albums_create(ui.clone());
    spawn_server_ping_loop(ui.clone());
    spawn_transfer_poll_loop(ui.clone());
    load_source_page(ui, initial_request, false);
}

fn spawn_server_ping_loop(ui: Rc<LibraryWindowUi>) {
    glib::timeout_add_seconds_local(5, move || {
        let ui_for_tick = ui.clone();
        glib::MainContext::default().spawn_local(async move {
            let _ = ui_for_tick.ctx.api_client.check_connection().await;
            let route = ui_for_tick.ctx.api_client.active_route_label().await;
            update_footer(&ui_for_tick, route);
        });
        glib::ControlFlow::Continue
    });
}

fn spawn_transfer_poll_loop(ui: Rc<LibraryWindowUi>) {
    glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
        let completed_batches = ui.ctx.state.lock().completed_upload_batches;
        if completed_batches != ui.last_seen_upload_batch.get() {
            ui.last_seen_upload_batch.set(completed_batches);
            refresh_library_after_mutation(ui.clone(), true);
        }
        update_transfer_ui(&ui);
        glib::ControlFlow::Continue
    });
}

fn fetch_current_user(ui: Rc<LibraryWindowUi>) {
    if ui.ctx.current_user_id.lock().is_some() {
        return;
    }
    glib::MainContext::default().spawn_local(async move {
        match ui.ctx.api_client.fetch_current_user_id().await {
            Ok(id) => {
                *ui.ctx.current_user_id.lock() = Some(id);
            }
            Err(err) => log::warn!("Could not fetch current user id: {}", err),
        }
    });
}

fn connect_albums_create(ui: Rc<LibraryWindowUi>) {
    ui.albums.create_button.connect_clicked(clone!(
        #[strong]
        ui,
        move |_| prompt_create_album(ui.clone())
    ));
}

fn prompt_create_album(ui: Rc<LibraryWindowUi>) {
    let dialog = libadwaita::AlertDialog::builder()
        .heading("Create album")
        .body("Choose a name for the new album.")
        .build();
    let entry = gtk::Entry::builder()
        .placeholder_text("Album name")
        .activates_default(true)
        .build();
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("create", "Create");
    dialog.set_response_appearance("create", libadwaita::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("create"));
    dialog.set_close_response("cancel");

    let ui_for_choice = ui.clone();
    let entry_for_choice = entry.clone();
    dialog.connect_response(None, move |dlg, response| {
        if response != "create" {
            return;
        }
        let name = entry_for_choice.text().to_string();
        if name.trim().is_empty() {
            return;
        }
        let ui = ui_for_choice.clone();
        glib::MainContext::default().spawn_local(async move {
            match ui.ctx.api_client.create_album(name.trim()).await {
                Ok(_) => {
                    refresh_library_after_mutation(ui.clone(), false);
                }
                Err(err) => log::error!("Create album failed: {}", err),
            }
        });
        dlg.close();
    });
    dialog.present(Some(&ui.window));
}

fn refresh_albums_view(ui: Rc<LibraryWindowUi>) {
    glib::MainContext::default().spawn_local(async move {
        match ui.ctx.api_client.fetch_library_albums().await {
            Ok(albums) => {
                let on_click = album_click_handler(ui.clone());
                populate_albums(&ui.albums, ui.ctx.clone(), albums, on_click);
                apply_narrow_recursive(ui.albums.root.upcast_ref(), ui.narrow.get());
            }
            Err(err) => log::warn!("Albums fetch failed: {}", err),
        }
    });
}

fn album_click_handler(ui: Rc<LibraryWindowUi>) -> AlbumClick {
    Rc::new(move |id: &str, name: String| {
        sidebar_dispatch(
            ui.clone(),
            LibrarySource::Album {
                id: id.to_string(),
                name,
            },
        );
    })
}

fn apply_timeline_ui_state(ui: &LibraryWindowUi, source: &LibrarySource) {
    let timeline_allowed = matches!(source, LibrarySource::AllAssets | LibrarySource::Timeline);
    let timeline_active = matches!(source, LibrarySource::Timeline);
    ui.ctx
        .library_timeline_active
        .store(timeline_active, std::sync::atomic::Ordering::Relaxed);
    ui.timeline_toggle.set_sensitive(timeline_allowed);
    if ui.timeline_toggle.is_active() != timeline_active {
        ui.timeline_toggle.set_active(timeline_active);
    }
    ui.timeline_banner.set_visible(timeline_active);
    if timeline_active {
        ui.sort_mode.set_selected(0);
    }

    let is_local = matches!(
        source,
        LibrarySource::LocalAll
            | LibrarySource::LocalSearch { .. }
            | LibrarySource::AlbumLocal { .. }
    );
    let is_unified = matches!(
        source,
        LibrarySource::Unified
            | LibrarySource::UnifiedSearch { .. }
            | LibrarySource::AlbumUnified { .. }
    );
    let remote_search_allowed = !is_local && !is_unified;
    ui.search_mode.set_visible(remote_search_allowed);
    ui.filters_button.set_visible(remote_search_allowed);
    if !remote_search_allowed {
        ui.search_mode.set_selected(0);
    }

    // Keep source dropdown visually consistent with the active source so
    // sidebar selections don't leave it showing the wrong tab.
    let target = if is_local {
        1
    } else if is_unified {
        2
    } else {
        0
    };
    if ui.source_mode.selected() != target {
        ui.source_mode_suppressed.set(true);
        ui.source_mode.set_selected(target);
        ui.source_mode_suppressed.set(false);
    }

    refresh_album_link_row(ui, source);
}

fn update_timeline_banner_if_active(ui: &Rc<LibraryWindowUi>, adj: &gtk::Adjustment) {
    let state = ui.ctx.library_state.lock();
    if !matches!(state.source, LibrarySource::Timeline) {
        return;
    }
    if state.assets.is_empty() {
        ui.timeline_banner.set_label("");
        return;
    }

    let max = (adj.upper() - adj.page_size()).max(1.0);
    let frac = (adj.value() / max).clamp(0.0, 1.0);
    let idx = ((state.assets.len() as f64) * frac) as usize;
    let idx = idx.min(state.assets.len() - 1);
    let label = month_year_label(&state.assets[idx].created_at);
    ui.timeline_banner.set_label(&label);
}

fn month_year_label(iso: &str) -> String {
    use chrono::{DateTime, Datelike};
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        const MONTHS: [&str; 12] = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        let m = dt.month0() as usize;
        if let Some(name) = MONTHS.get(m) {
            return format!("{} {}", name, dt.year());
        }
    }
    iso.chars().take(7).collect()
}

fn load_albums(ui: Rc<LibraryWindowUi>) {
    glib::MainContext::default().spawn_local(clone!(
        #[strong]
        ui,
        async move {
            match ui.ctx.api_client.fetch_library_albums().await {
                Ok(albums) => {
                    ui.ctx.library_state.lock().load_albums(albums);
                    reload_sidebar(&ui);
                }
                Err(err) => {
                    ui.error_label
                        .set_label(&format!("Could not load albums: {}", err));
                    ui.content_stack.set_visible_child_name("error");
                }
            }
        }
    ));
}

fn load_status(ui: Rc<LibraryWindowUi>) {
    glib::MainContext::default().spawn_local(clone!(
        #[strong]
        ui,
        async move {
            let stats = ui.ctx.api_client.fetch_server_stats().await.ok();
            let about = ui.ctx.api_client.fetch_server_about().await.ok();
            let route = ui.ctx.api_client.active_route_label().await;

            {
                let mut state = ui.ctx.library_state.lock();
                state.set_status(stats, about);
            }
            update_footer(&ui, route);
        }
    ));
}

fn load_explore_landing(ui: Rc<LibraryWindowUi>) {
    ui.content_stack.set_visible_child_name("explore");
    if ui.explore.populated.get() {
        return;
    }
    ui.explore.populated.set(true);
    let ctx = ui.ctx.clone();

    glib::MainContext::default().spawn_local(clone!(
        #[strong]
        ui,
        async move {
            let people = ctx.api_client.fetch_people().await.unwrap_or_default();
            let sections = ctx.api_client.fetch_explore().await.unwrap_or_default();
            let places = ctx.api_client.fetch_all_places().await.unwrap_or_default();

            let click_ui = ui.clone();
            explore_view::populate_people(&ui.explore, ctx.clone(), people, move |id, name| {
                let filters = MetadataSearchFilters {
                    person_ids: Some(vec![id]),
                    ..Default::default()
                };
                let request = click_ui.ctx.library_state.lock().switch_source(
                    LibrarySource::AdvancedSearch {
                        filters: Box::new(filters),
                    },
                );
                click_ui.search_entry.set_text(&name);
                apply_timeline_ui_state(&click_ui, &request.1);
                load_source_page(click_ui.clone(), request, false);
            });
            let click_ui = ui.clone();
            explore_view::populate_places(&ui.explore, ctx.clone(), places, move |_kind, value| {
                let next = LibrarySource::AdvancedSearch {
                    filters: Box::new(MetadataSearchFilters {
                        city: Some(value.clone()),
                        ..Default::default()
                    }),
                };
                let request = click_ui.ctx.library_state.lock().switch_source(next);
                click_ui.search_entry.set_text(&value);
                apply_timeline_ui_state(&click_ui, &request.1);
                load_source_page(click_ui.clone(), request, false);
            });
            let click_ui = ui.clone();
            explore_view::populate_explore(
                &ui.explore,
                ctx.clone(),
                sections,
                move |_kind, value| {
                    let next = LibrarySource::SmartSearch {
                        query: value.clone(),
                    };
                    let request = click_ui.ctx.library_state.lock().switch_source(next);
                    click_ui.search_entry.set_text(&value);
                    apply_timeline_ui_state(&click_ui, &request.1);
                    load_source_page(click_ui.clone(), request, false);
                },
            );
            apply_narrow_recursive(ui.explore.root.upcast_ref(), ui.narrow.get());
        }
    ));
}

fn load_source_page(ui: Rc<LibraryWindowUi>, request: (u64, LibrarySource, u32), append: bool) {
    if matches!(request.1, LibrarySource::Explore) {
        load_explore_landing(ui);
        return;
    }
    if !append {
        ui.content_stack.set_visible_child_name("loading");
    }
    glib::MainContext::default().spawn_local(clone!(
        #[strong]
        ui,
        async move {
            let (generation, source, page) = request;
            let order = ui.ctx.library_state.lock().sort_mode.server_order();
            let result: Result<(Vec<LibraryAsset>, bool), String> = match source.clone() {
                LibrarySource::AllAssets | LibrarySource::Timeline => {
                    ui.ctx
                        .api_client
                        .search_metadata("", page, PAGE_SIZE, order)
                        .await
                }
                LibrarySource::Explore => unreachable!("intercepted above"),
                LibrarySource::Album { id, .. } => {
                    ui.ctx
                        .api_client
                        .fetch_album_assets(&id, page, PAGE_SIZE, order)
                        .await
                }
                LibrarySource::SmartSearch { query } => {
                    ui.ctx
                        .api_client
                        .search_smart(&query, page, PAGE_SIZE)
                        .await
                }
                LibrarySource::OcrSearch { query } => {
                    ui.ctx
                        .api_client
                        .search_ocr(&query, page, PAGE_SIZE, order)
                        .await
                }
                LibrarySource::MetadataSearch { query } => {
                    ui.ctx
                        .api_client
                        .search_metadata(&query, page, PAGE_SIZE, order)
                        .await
                }
                LibrarySource::AdvancedSearch { filters } => {
                    let mut filters = (*filters).clone();
                    filters.order = order;
                    ui.ctx
                        .api_client
                        .search_metadata_with_filters(&filters, page, PAGE_SIZE)
                        .await
                }
                LibrarySource::LocalAll => {
                    // Local enumeration is bounded — single synthetic page.
                    if page > 1 {
                        Ok((Vec::new(), false))
                    } else {
                        let locals = enumerate_local(ui.ctx.clone()).await;
                        Ok((
                            locals.into_iter().map(local_to_library_asset).collect(),
                            false,
                        ))
                    }
                }
                LibrarySource::LocalSearch { query } => {
                    if page > 1 {
                        Ok((Vec::new(), false))
                    } else {
                        let locals = enumerate_local(ui.ctx.clone()).await;
                        let filtered = filter_by_filename(locals, &query);
                        Ok((
                            filtered.into_iter().map(local_to_library_asset).collect(),
                            false,
                        ))
                    }
                }
                LibrarySource::Unified => {
                    let remote = ui
                        .ctx
                        .api_client
                        .search_metadata("", page, PAGE_SIZE, order)
                        .await;
                    merge_unified_page(remote, page, &ui, None).await
                }
                LibrarySource::UnifiedSearch { query } => {
                    let remote = ui
                        .ctx
                        .api_client
                        .search_metadata(&query, page, PAGE_SIZE, order)
                        .await;
                    merge_unified_page(remote, page, &ui, Some(&query)).await
                }
                LibrarySource::AlbumLocal { name, .. } => {
                    if page > 1 {
                        Ok((Vec::new(), false))
                    } else {
                        match linked_entry_path_for_album(&ui, &name) {
                            Some(path) => {
                                let locals = enumerate_local_for_entry(ui.ctx.clone(), path).await;
                                Ok((
                                    locals.into_iter().map(local_to_library_asset).collect(),
                                    false,
                                ))
                            }
                            None => Ok((Vec::new(), false)),
                        }
                    }
                }
                LibrarySource::AlbumUnified { id, name } => {
                    let remote = ui
                        .ctx
                        .api_client
                        .fetch_album_assets(&id, page, PAGE_SIZE, order)
                        .await;
                    merge_album_unified_page(remote, page, &ui, &name).await
                }
            };

            match result {
                Ok((items, has_more)) => {
                    {
                        let mut state = ui.ctx.library_state.lock();
                        let applied = if append {
                            state.append_assets_with_more(generation, items, has_more)
                        } else {
                            state.replace_assets_with_more(generation, items, has_more)
                        };
                        if !applied {
                            return;
                        }
                        if append {
                            ui.grid
                                .model
                                .extend(&ui.ctx, &state.assets, &state.sort_mode);
                        } else {
                            ui.grid
                                .model
                                .reset(&ui.ctx, &state.assets, &state.sort_mode);
                        }
                    }
                    sync_content_state(&ui);
                    reload_sidebar(&ui);
                    update_timeline_banner_if_active(&ui, &ui.grid.scrolled.vadjustment());
                }
                Err(err) => {
                    let mut state = ui.ctx.library_state.lock();
                    state.mark_error(generation, err.clone());
                    ui.error_label
                        .set_label(&format!("Could not load library assets: {}", err));
                    ui.content_stack.set_visible_child_name("error");
                }
            }
        }
    ));
}

fn reload_sidebar(ui: &Rc<LibraryWindowUi>) {
    while let Some(row) = ui.sidebar.albums_list.first_child() {
        ui.sidebar.albums_list.remove(&row);
    }

    let selected_source = ui.ctx.library_state.lock().source.clone();
    let albums = ui.ctx.library_state.lock().albums.clone();
    for album in albums {
        let subtitle = format!("{} asset(s)", album.asset_count);
        let action = libadwaita::ActionRow::builder()
            .title(&album.album_name)
            .subtitle(&subtitle)
            .title_lines(1)
            .subtitle_lines(1)
            .build();
        let row = gtk::ListBoxRow::builder()
            .tooltip_text(format!("{}:{}", album.id, album.album_name))
            .child(&action)
            .build();
        ui.sidebar.albums_list.append(&row);
    }

    match selected_source {
        LibrarySource::Timeline => {
            select_fixed_row(&ui.sidebar.fixed_list, "photos");
            ui.sidebar.albums_list.unselect_all();
        }
        LibrarySource::Explore => {
            select_fixed_row(&ui.sidebar.fixed_list, "explore");
            ui.sidebar.albums_list.unselect_all();
        }
        LibrarySource::Album { id, .. }
        | LibrarySource::AlbumLocal { id, .. }
        | LibrarySource::AlbumUnified { id, .. } => {
            ui.sidebar.fixed_list.unselect_all();
            ui.sidebar_suppressed.set(true);
            let mut child = ui.sidebar.albums_list.first_child();
            while let Some(widget) = child {
                let next = widget.next_sibling();
                if let Ok(row) = widget.downcast::<gtk::ListBoxRow>()
                    && row.tooltip_text().as_deref().is_some_and(|tooltip| {
                        tooltip.split_once(':').map(|(prefix, _)| prefix) == Some(id.as_str())
                    })
                {
                    ui.sidebar.albums_list.select_row(Some(&row));
                    ui.sidebar_suppressed.set(false);
                    break;
                }
                child = next;
            }
            ui.sidebar_suppressed.set(false);
        }
        _ => {
            ui.sidebar.fixed_list.unselect_all();
            ui.sidebar.albums_list.unselect_all();
        }
    }
}

fn select_fixed_row(list: &gtk::ListBox, key: &str) {
    let mut child = list.first_child();
    while let Some(widget) = child {
        let next = widget.next_sibling();
        if let Ok(row) = widget.downcast::<gtk::ListBoxRow>()
            && row.tooltip_text().as_deref() == Some(key)
        {
            list.select_row(Some(&row));
            return;
        }
        child = next;
    }
}

fn sync_content_state(ui: &LibraryWindowUi) {
    match &ui.ctx.library_state.lock().load_state {
        LibraryLoadState::Idle | LibraryLoadState::Loading => {
            ui.content_stack.set_visible_child_name("loading");
        }
        LibraryLoadState::Loaded => {
            ui.content_stack.set_visible_child_name("grid");
        }
        LibraryLoadState::Empty => {
            ui.content_stack.set_visible_child_name("empty");
        }
        LibraryLoadState::Error(message) => {
            ui.error_label.set_label(message);
            ui.content_stack.set_visible_child_name("error");
        }
    }
}

fn update_footer(ui: &LibraryWindowUi, route: Option<String>) {
    let state = ui.ctx.library_state.lock();
    let route_subtitle = route
        .as_deref()
        .map(|route| match route {
            "LAN" => "Connected through LAN",
            "WAN" => "Connected through WAN",
            _ => "Connected through configured server",
        })
        .unwrap_or("Offline");
    ui.sidebar.connection_row.set_subtitle(route_subtitle);

    let stats = state
        .status
        .stats
        .as_ref()
        .map(|stats| format!("{} photos, {} videos", stats.images, stats.videos))
        .unwrap_or_else(|| "Statistics unavailable".to_string());
    let about = state
        .status
        .about
        .as_ref()
        .map(|about| format!("Immich {}", about.version))
        .unwrap_or_else(|| "Version unavailable".to_string());
    ui.sidebar
        .server_row
        .set_subtitle(&format!("{stats} | {about}"));
}

fn update_transfer_ui(ui: &LibraryWindowUi) {
    let transfer = {
        let mut state = ui.ctx.state.lock();
        if state.transfer.active
            && state.transfer.active_uploads == 0
            && state.transfer.active_downloads == 0
        {
            // Guard against sessions that were opened but never queued.
            state.transfer.reset_runtime();
        }
        state.transfer.clone()
    };
    if !transfer.active {
        ui.transfer_bar.remove_css_class("active");
        ui.transfer_progress.set_fraction(0.0);
        ui.transfer_icon.set_visible(false);
        let idle_summary =
            if transfer.last_upload_avg_bps > 0.0 || transfer.last_download_avg_bps > 0.0 {
                format!(
                    "Idle  Last upload avg {}  Last download avg {}",
                    format_rate(transfer.last_upload_avg_bps),
                    format_rate(transfer.last_download_avg_bps)
                )
            } else {
                "Idle  No recent transfer session".to_string()
            };
        ui.transfer_label.set_label(&idle_summary);
        return;
    }
    ui.transfer_bar.add_css_class("active");

    let icon_name = match transfer.direction {
        TransferDirection::Upload => "mimick-upload-symbolic",
        TransferDirection::Download => "mimick-download-symbolic",
    };
    ui.transfer_icon.set_icon_name(Some(icon_name));
    ui.transfer_icon.set_visible(true);

    let detail = transfer
        .active_item_label
        .as_deref()
        .unwrap_or(match transfer.direction {
            TransferDirection::Upload => "queued asset",
            TransferDirection::Download => "selected asset",
        });
    let live_speed = format_rate(transfer.instant_bps);
    let avg_speed = format_rate(transfer.session_avg_bps);
    ui.transfer_label
        .set_label(&format!("{detail}  {live_speed}  avg {avg_speed}"));

    match transfer.total_bytes {
        Some(total) if total > 0 => {
            ui.transfer_progress.set_show_text(false);
            ui.transfer_progress
                .set_fraction((transfer.current_bytes as f64 / total as f64).clamp(0.0, 1.0));
        }
        _ => {
            ui.transfer_progress.pulse();
        }
    }
}

fn build_status_view(icon_name: &str, title: &str, subtitle: &str) -> gtk::Box {
    let container = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .vexpand(true)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .css_classes(vec!["mimick-empty".to_string()])
        .build();
    let icon = gtk::Image::builder()
        .icon_name(icon_name)
        .pixel_size(64)
        .build();
    icon.add_css_class("dim-label");
    let title_label = gtk::Label::builder()
        .label(title)
        .css_classes(vec!["mimick-empty-title".to_string()])
        .build();
    let subtitle_label = gtk::Label::builder()
        .label(subtitle)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(vec!["mimick-empty-subtitle".to_string()])
        .build();
    container.append(&icon);
    container.append(&title_label);
    container.append(&subtitle_label);
    container
}

/// Walk the widget tree and resize known cards/panes for narrow viewports.
pub(super) fn apply_narrow_recursive(widget: &gtk::Widget, narrow: bool) {
    if let Some(pic) = widget.downcast_ref::<gtk::Picture>() {
        if pic.has_css_class("mimick-grid-thumb") {
            pic.set_width_request(if narrow { 160 } else { 356 });
            pic.set_height_request(if narrow { 120 } else { 200 });
        } else if pic.has_css_class("mimick-explore-tile") {
            pic.set_width_request(if narrow { 160 } else { 300 });
            pic.set_height_request(if narrow { 140 } else { 220 });
        }
    } else if let Some(sw) = widget.downcast_ref::<gtk::ScrolledWindow>()
        && sw.has_css_class("mimick-details-pane")
    {
        sw.set_min_content_width(if narrow { 260 } else { 320 });
        sw.set_max_content_width(if narrow { 260 } else { 320 });
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        apply_narrow_recursive(&c, narrow);
        child = c.next_sibling();
    }
}

pub(super) fn immich_checksum_to_hex(b64: &str) -> Option<String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    Some(bytes.iter().map(|b| format!("{:02x}", b)).collect())
}

fn local_to_library_asset(local: LocalAsset) -> LibraryAsset {
    LibraryAsset {
        id: format!("{}{}", LOCAL_ID_PREFIX, local.path.display()),
        filename: local.filename,
        mime_type: local.mime,
        created_at: local.created_at,
        asset_type: local.asset_type.to_string(),
        thumbhash: None,
        width: None,
        height: None,
        checksum: None,
    }
}

async fn merge_unified_page(
    remote: Result<(Vec<LibraryAsset>, bool), String>,
    page: u32,
    ui: &Rc<LibraryWindowUi>,
    query: Option<&str>,
) -> Result<(Vec<LibraryAsset>, bool), String> {
    let (mut remote, has_more) = remote?;
    if page > 1 {
        return Ok((remote, has_more));
    }

    let mut locals = enumerate_local(ui.ctx.clone()).await;
    if let Some(q) = query {
        locals = filter_by_filename(locals, q);
    }

    let synced_paths: std::collections::HashSet<String> = {
        remote
            .iter()
            .filter_map(|a| a.checksum.as_deref())
            .filter_map(immich_checksum_to_hex)
            .filter_map(|hex| ui.ctx.sync_index.local_path_for_checksum(&hex))
            .collect()
    };

    let mut local_rows: Vec<LibraryAsset> = locals
        .into_iter()
        .filter(|l| !synced_paths.contains(&l.path.display().to_string()))
        .map(local_to_library_asset)
        .collect();

    local_rows.append(&mut remote);
    Ok((local_rows, has_more))
}

/// Return the path of the watch entry linked to `album_name`, if any.
fn linked_entry_path_for_album(ui: &Rc<LibraryWindowUi>, album_name: &str) -> Option<String> {
    let entries = ui.ctx.live_watch_paths.lock().clone();
    crate::config::watch_entry_for_album(album_name, &entries).map(|e| e.path().to_string())
}

/// Album-scoped variant of `merge_unified_page`: takes the album's asset
/// page from the remote API and overlays sync state from the album's
/// linked local folder only — never from siblings.
async fn merge_album_unified_page(
    remote: Result<(Vec<LibraryAsset>, bool), String>,
    page: u32,
    ui: &Rc<LibraryWindowUi>,
    album_name: &str,
) -> Result<(Vec<LibraryAsset>, bool), String> {
    let (mut remote, has_more) = remote?;
    if page > 1 {
        return Ok((remote, has_more));
    }

    let locals = match linked_entry_path_for_album(ui, album_name) {
        Some(path) => enumerate_local_for_entry(ui.ctx.clone(), path).await,
        None => Vec::new(),
    };

    let synced_paths: std::collections::HashSet<String> = {
        remote
            .iter()
            .filter_map(|a| a.checksum.as_deref())
            .filter_map(immich_checksum_to_hex)
            .filter_map(|hex| ui.ctx.sync_index.local_path_for_checksum(&hex))
            .collect()
    };

    let mut local_rows: Vec<LibraryAsset> = locals
        .into_iter()
        .filter(|l| !synced_paths.contains(&l.path.display().to_string()))
        .map(local_to_library_asset)
        .collect();

    local_rows.append(&mut remote);
    Ok((local_rows, has_more))
}
