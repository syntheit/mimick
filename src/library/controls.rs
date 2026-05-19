//! Header bar button wiring, search, sort, source-mode, timeline, and sidebar/grid handlers.

use std::rc::Rc;

use glib::clone;
use gtk::prelude::*;

use crate::library::asset_object::AssetObject;
use crate::library::state::{LibrarySortMode, LibrarySource};
use crate::settings_window::{build_settings_window_with_parent, show_queue_inspector};

use super::download::{open_local_with_default_app, spawn_video_handoff};
use super::{
    LibraryWindowUi, apply_timeline_ui_state, load_albums, load_source_page, load_status,
    open_lightbox, refresh_albums_view, update_timeline_banner_if_active,
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

pub(super) fn refresh_library_surfaces(ui: Rc<LibraryWindowUi>, include_current_source: bool) {
    load_albums(ui.clone());
    load_status(ui.clone());
    ui.explore.populated.set(false);
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

pub(super) fn connect_sidebar_handlers(ui: Rc<LibraryWindowUi>) {
    // Photos / Explore (fixed destinations).
    ui.sidebar.fixed_list.connect_row_selected(clone!(
        #[strong]
        ui,
        move |_, row| {
            let Some(row) = row else {
                return;
            };
            let key = row.tooltip_text().unwrap_or_default();
            ui.sidebar.albums_list.unselect_all();
            match key.as_str() {
                "photos" => sidebar_dispatch(ui.clone(), LibrarySource::Timeline),
                "explore" => sidebar_dispatch(ui.clone(), LibrarySource::Explore),
                "albums" => {
                    ui.album_link_row.set_visible(false);
                    if let Some(parent) = ui.album_link_row.parent() {
                        parent.set_visible(false);
                    }
                    ui.content_stack.set_visible_child_name("albums");
                    refresh_albums_view(ui.clone());
                }
                _ => {}
            }
            auto_hide_sidebar(&ui);
        }
    ));

    ui.sidebar.albums_list.connect_row_selected(clone!(
        #[strong]
        ui,
        move |_, row| {
            if ui.sidebar_suppressed.get() {
                return;
            }
            let Some(row) = row else {
                return;
            };
            let Some(tooltip) = row.tooltip_text() else {
                return;
            };
            let mut parts = tooltip.splitn(2, ':');
            let id = parts.next().unwrap_or_default().to_string();
            let name = parts.next().unwrap_or("Album").to_string();
            ui.sidebar.fixed_list.unselect_all();
            sidebar_dispatch(ui.clone(), LibrarySource::Album { id, name });
            auto_hide_sidebar(&ui);
        }
    ));
}

fn auto_hide_sidebar(ui: &Rc<LibraryWindowUi>) {
    if ui.split.is_collapsed() {
        ui.split.set_show_sidebar(false);
    }
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

/// Common path for sidebar selections. Skips redundant dispatches so the
/// `reload_sidebar` programmatic re-selection doesn't loop into another
/// fetch -- but only when the content stack is already showing the current
/// source, since the Albums grid moves the stack without changing source.
pub(super) fn sidebar_dispatch(ui: Rc<LibraryWindowUi>, source: LibrarySource) {
    let on_albums_grid = ui.content_stack.visible_child_name().as_deref() == Some("albums");
    if !on_albums_grid && ui.ctx.library_state.lock().source == source {
        return;
    }
    let request = ui.ctx.library_state.lock().navigate_to(source);
    apply_timeline_ui_state(&ui, &request.1);
    load_source_page(ui.clone(), request, false);
    update_back_button(&ui);
}

pub(super) fn connect_grid_handlers(ui: Rc<LibraryWindowUi>) {
    ui.grid.view.connect_activate(clone!(
        #[strong]
        ui,
        move |_, position| {
            let Some(item) = ui.grid.model.item(position).and_downcast::<AssetObject>() else {
                return;
            };
            let asset_id = item.property::<String>("id");
            let filename = item.property::<String>("filename");
            let local_path = item.property::<String>("local-path");
            let asset_type = item.property::<String>("asset-type");

            // Videos open in the system default player per spec -- no in-app
            // playback for v1.
            if asset_type.eq_ignore_ascii_case("VIDEO") {
                if !local_path.is_empty() {
                    open_local_with_default_app(&local_path);
                } else {
                    spawn_video_handoff(ui.clone(), asset_id, filename);
                }
                return;
            }

            open_lightbox(ui.clone(), position);
        }
    ));

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
