//! Header bar button wiring, search, sort, source-mode, timeline, tab-switch, and grid handlers.
//!
//! Connects signal handlers for the library header bar controls including
//! the search entry, sort-mode dropdown, view-source toggle, and timeline
//! scrubber, plus the bottom-nav tab drill-in that reparents the shared
//! photos grid into pushed detail pages.

use std::cell::RefCell;
use std::rc::Rc;

use glib::clone;
use gtk::prelude::*;

use crate::library::asset_object::AssetObject;
use crate::library::state::{LibrarySortMode, LibrarySource};
use crate::settings_window::{build_settings_window_with_parent, show_queue_inspector};

use super::shell;
use super::{
    LibraryWindowUi, apply_timeline_ui_state, load_albums, load_source_page, load_status,
    open_lightbox, update_timeline_banner_if_active,
};

pub(super) fn connect_controls(ui: Rc<LibraryWindowUi>) {
    let action_settings = gtk::gio::SimpleAction::new("settings", None);
    action_settings.connect_activate(clone!(
        #[strong]
        ui,
        move |_, _| {
            build_settings_window_with_parent(&ui.app, ui.ctx.clone(), Some(&ui.window));
        }
    ));
    ui.window.add_action(&action_settings);

    let action_queue = gtk::gio::SimpleAction::new("queue", None);
    action_queue.connect_activate(clone!(
        #[strong]
        ui,
        move |_, _| {
            show_queue_inspector(&ui.window, ui.ctx.queue_manager.clone());
        }
    ));
    ui.window.add_action(&action_queue);

    let action_refresh = gtk::gio::SimpleAction::new("refresh", None);
    action_refresh.connect_activate(clone!(
        #[strong]
        ui,
        move |_, _| {
            refresh_library_surfaces(ui.clone(), true);
        }
    ));
    ui.window.add_action(&action_refresh);

    ui.upload_button.connect_clicked(clone!(
        #[strong]
        ui,
        move |_| {
            let album = match ui.ctx.library_state.lock().source.clone() {
                LibrarySource::Album { id, name }
                | LibrarySource::AlbumLocal { id, name }
                | LibrarySource::AlbumUnified { id, name } => Some((id, name)),
                _ => None,
            };
            super::upload_picker::pick_and_upload(&ui.window, ui.ctx.clone(), album);
        }
    ));

    ui.back_button.connect_clicked(clone!(
        #[strong]
        ui,
        move |_| {
            navigate_back(ui.clone());
        }
    ));

    // Alt+Left -> back. Attached to the window so it works regardless of focus.
    let alt_left = gtk::Shortcut::builder()
        .trigger(&gtk::ShortcutTrigger::parse_string("<Alt>Left").unwrap())
        .action(&gtk::CallbackAction::new(clone!(
            #[strong]
            ui,
            move |_, _| {
                if navigate_back(ui.clone()) {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            }
        )))
        .build();
    let alt_left_controller = gtk::ShortcutController::new();
    alt_left_controller.add_shortcut(alt_left);
    ui.window.add_controller(alt_left_controller);

    let f5_controller = gtk::EventControllerKey::new();
    f5_controller.connect_key_pressed(clone!(
        #[strong]
        ui,
        move |_, keyval, _, _| {
            if keyval == gtk::gdk::Key::F5 {
                let _ = ui
                    .window
                    .upcast_ref::<gtk::Widget>()
                    .activate_action("win.refresh", None);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    ));
    ui.window.add_controller(f5_controller);

    ui.source_mode.connect_selected_notify(clone!(
        #[strong]
        ui,
        move |dropdown| {
            if ui.source_mode_suppressed.get() {
                return;
            }
            let album_ctx = match ui.ctx.library_state.lock().source.clone() {
                LibrarySource::Album { id, name }
                | LibrarySource::AlbumLocal { id, name }
                | LibrarySource::AlbumUnified { id, name } => Some((id, name)),
                _ => None,
            };
            let source = match (dropdown.selected(), album_ctx) {
                (1, Some((id, name))) => LibrarySource::AlbumLocal { id, name },
                (1, None) => LibrarySource::LocalAll,
                (2, Some((id, name))) => LibrarySource::AlbumUnified { id, name },
                (2, None) => LibrarySource::Unified,
                (_, Some((id, name))) => LibrarySource::Album { id, name },
                (_, None) => {
                    if ui.timeline_toggle.is_active() {
                        LibrarySource::Timeline
                    } else {
                        LibrarySource::AllAssets
                    }
                }
            };
            // Searching while switching sources would require thread-safe
            // re-routing of the search field; clear it on source change.
            ui.search_entry.set_text("");
            let request = ui.ctx.library_state.lock().navigate_to(source);
            apply_timeline_ui_state(&ui, &request.1);
            load_source_page(ui.clone(), request, false);
            update_back_button(&ui);
        }
    ));

    ui.timeline_toggle.connect_toggled(clone!(
        #[strong]
        ui,
        move |toggle| {
            if !toggle.is_sensitive() {
                return;
            }
            let current = ui.ctx.library_state.lock().source.clone();
            if !matches!(current, LibrarySource::AllAssets | LibrarySource::Timeline) {
                toggle.set_active(false);
                return;
            }
            if matches!(current, LibrarySource::Timeline) == toggle.is_active() {
                return;
            }
            ui.search_entry.set_text("");
            let next_source = if toggle.is_active() {
                LibrarySource::Timeline
            } else {
                LibrarySource::AllAssets
            };
            let request = ui.ctx.library_state.lock().navigate_to(next_source);
            apply_timeline_ui_state(&ui, &request.1);
            load_source_page(ui.clone(), request, false);
            update_back_button(&ui);
        }
    ));

    ui.search_entry.connect_activate(clone!(
        #[strong]
        ui,
        move |entry| {
            // The search field lives on the Photos tab and drives asset
            // search. (Albums/Library in-memory filtering is a later stage.)
            let query = entry.text().trim().to_string();
            if query.is_empty() {
                return;
            }

            let source = match ui.source_mode.selected() {
                1 => LibrarySource::LocalSearch { query },
                2 => LibrarySource::UnifiedSearch { query },
                _ => match ui.search_mode.selected() {
                    1 => LibrarySource::SmartSearch { query },
                    2 => LibrarySource::OcrSearch { query },
                    _ => LibrarySource::MetadataSearch { query },
                },
            };
            let request = ui.ctx.library_state.lock().navigate_to(source);
            apply_timeline_ui_state(&ui, &request.1);
            load_source_page(ui.clone(), request, false);
            update_back_button(&ui);
        }
    ));

    ui.search_mode.connect_selected_notify(clone!(
        #[strong]
        ui,
        move |dropdown| {
            let placeholder = match dropdown.selected() {
                1 => "Describe what you're looking for\u{2026}",
                2 => "Find words shown inside images",
                _ => "Search filenames",
            };
            ui.search_entry.set_placeholder_text(Some(placeholder));
        }
    ));

    ui.search_entry.connect_stop_search(clone!(
        #[strong]
        ui,
        move |entry| {
            entry.set_text("");
            let request = ui
                .ctx
                .library_state
                .lock()
                .clear_search_restore_previous_source();
            if let Some(request) = request {
                apply_timeline_ui_state(&ui, &request.1);
                load_source_page(ui.clone(), request, false);
            }
        }
    ));

    ui.sort_mode.connect_selected_notify(clone!(
        #[strong]
        ui,
        move |dropdown| {
            // The sort dropdown lives on the Photos tab and sorts the asset
            // grid. (Albums has its own sort taxonomy, applied when that tab
            // grows its own controls in a later stage.)
            let sort_mode = match dropdown.selected() {
                1 => LibrarySortMode::Filename,
                2 => LibrarySortMode::FileType,
                _ => LibrarySortMode::NewestFirst,
            };

            let mut state = ui.ctx.library_state.lock();
            state.apply_sort(sort_mode);
            ui.grid
                .model
                .reset(&ui.ctx, &state.assets, &state.sort_mode);
        }
    ));
}

/// Refresh the search/sort context to match the visible tab (called on tab
/// switch). Stage 1: the shared search/sort controls live on the Photos tab,
/// so this only resets the placeholder for the Photos context. Albums/Library
/// filtering wires their own controls in a later stage.
pub(super) fn sync_tab_controls(ui: &Rc<LibraryWindowUi>) {
    let tab = ui.view_stack.visible_child_name();
    if tab.as_deref() == Some(shell::TAB_PHOTOS) {
        ui.search_entry
            .set_placeholder_text(Some("Search filenames"));
    }
    // Clear any stale in-memory list filters when leaving a landing tab so a
    // revisit shows the full list.
    super::albums_view::set_search_filter(&ui.albums, "");
    super::explore_view::set_people_search(&ui.explore, "");
}

/// Drill into a filtered/album grid: push a detail page onto `nav`, reparent
/// the shared photos grid into it, and load `source`. On pop, reparent the
/// grid back to the Photos tab. Reused by album clicks (Albums tab) and
/// people/places/things clicks (Library tab).
pub(super) fn tab_drill_in(
    ui: Rc<LibraryWindowUi>,
    nav: libadwaita::NavigationView,
    title: String,
    source: LibrarySource,
) {
    let drill = shell::DrillPage::new(&title);

    // Move the shared grid scrolled window into the drill page's content slot.
    shell::unparent_from_slot(&ui.grid_scrolled);
    drill.content_slot.append(&ui.grid_scrolled);
    drill.show_loading();
    *ui.active_drill.borrow_mut() = Some(drill.clone());

    // When this page is popped (swipe-back / header back), return the grid to
    // the Photos tab and clear the active-drill target.
    let ui_for_pop = ui.clone();
    let drill_page = drill.page.clone();
    let handler: Rc<RefCell<Option<glib::SignalHandlerId>>> = Rc::new(RefCell::new(None));
    let handler_clone = handler.clone();
    let id = nav.connect_popped(move |nav, page| {
        if page != &drill_page {
            return;
        }
        // Reparent the grid back onto the Photos tab content.
        return_grid_to_photos(&ui_for_pop);
        *ui_for_pop.active_drill.borrow_mut() = None;
        if let Some(id) = handler_clone.borrow_mut().take() {
            nav.disconnect(id);
        }
    });
    *handler.borrow_mut() = Some(id);

    nav.push(&drill.page);

    let request = ui.ctx.library_state.lock().navigate_to(source);
    apply_timeline_ui_state(&ui, &request.1);
    load_source_page(ui.clone(), request, false);
    update_back_button(&ui);
}

/// Return the shared grid scrolled window to the Photos tab's stable grid host.
fn return_grid_to_photos(ui: &Rc<LibraryWindowUi>) {
    shell::unparent_from_slot(&ui.grid_scrolled);
    ui.grid_host.append(&ui.grid_scrolled);
}

pub(super) fn refresh_library_surfaces(ui: Rc<LibraryWindowUi>, include_current_source: bool) {
    // load_albums repopulates the Albums tab landing on its own schedule.
    load_albums(ui.clone());
    load_status(ui.clone());

    // Clearing `populated` marks the cached explore grid as stale but, by
    // itself, won't repaint a visible tab. Re-trigger the loader for whichever
    // tab is currently on-screen so the user sees fresh content (with
    // spinners) instead of silently-stale tiles.
    let tab = ui.view_stack.visible_child_name();
    ui.explore.populated.set(false);
    match tab.as_deref() {
        Some(shell::TAB_LIBRARY) => super::load_explore_landing(ui.clone()),
        Some(shell::TAB_ALBUMS) => super::refresh_albums_view(ui.clone()),
        _ => {}
    }

    if include_current_source {
        let request = {
            let source = ui.ctx.library_state.lock().source.clone();
            ui.ctx.library_state.lock().switch_source(source)
        };
        load_source_page(ui, request, false);
    }
}

pub(super) fn refresh_library_after_mutation(ui: Rc<LibraryWindowUi>, prefer_current_source: bool) {
    refresh_library_surfaces(ui, prefer_current_source);
}

pub(super) fn update_back_button(ui: &Rc<LibraryWindowUi>) {
    let can_go_back = ui.ctx.library_state.lock().can_go_back();
    ui.back_button.set_sensitive(can_go_back);
}

/// Pop the navigation history and switch to the previous source. Returns
/// true if a back-step was taken, false when the history was empty.
pub(super) fn navigate_back(ui: Rc<LibraryWindowUi>) -> bool {
    ui.search_entry.set_text("");
    let request = ui.ctx.library_state.lock().navigate_back();
    if let Some(request) = request {
        apply_timeline_ui_state(&ui, &request.1);
        load_source_page(ui.clone(), request, false);
        update_back_button(&ui);
        true
    } else {
        false
    }
}

pub(super) fn connect_grid_handlers(ui: Rc<LibraryWindowUi>) {
    let ui_for_activate = ui.clone();
    ui.grid.canvas.set_activate_handler(move |position| {
        if ui_for_activate
            .grid
            .model
            .item(position)
            .and_downcast::<AssetObject>()
            .is_none()
        {
            return;
        }
        // Both images and videos open in the lightbox; the lightbox plays
        // videos inline via gtk::Video (streaming or local file).
        open_lightbox(ui_for_activate.clone(), position);
    });

    let scroll_pending = Rc::new(std::cell::Cell::new(false));
    ui.grid.scrolled.vadjustment().connect_value_changed(clone!(
        #[strong]
        ui,
        #[strong]
        scroll_pending,
        move |_adj| {
            if scroll_pending.replace(true) {
                return;
            }
            let ui = ui.clone();
            let scroll_pending = scroll_pending.clone();
            glib::idle_add_local_once(move || {
                scroll_pending.set(false);
                let adj = ui.grid.scrolled.vadjustment();
                update_timeline_banner_if_active(&ui, &adj);

                let threshold = (adj.upper() - adj.page_size()) * 0.50;
                if adj.value() < threshold {
                    return;
                }

                let next = ui.ctx.library_state.lock().load_next_page_if_needed();
                if let Some(request) = next {
                    load_source_page(ui.clone(), request, true);
                }
            });
        }
    ));
}
