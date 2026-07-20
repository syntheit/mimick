//! Selection-mode wiring and bulk batch operations.
//!
//! Manages multi-select mode toggling, selection-count tracking, and
//! batch actions such as bulk download, bulk delete-to-trash, and
//! bulk add-to-album for the library grid.

use std::cell::Cell;
use std::rc::Rc;

use glib::clone;
use gtk::prelude::*;
use libadwaita::prelude::*;

use crate::library::asset_object::AssetObject;

use super::LOCAL_ID_PREFIX;
use super::LibraryWindowUi;
use super::download::start_download_group;

pub(super) fn connect_select_mode(ui: Rc<LibraryWindowUi>, select_toggle: gtk::ToggleButton) {
    let selection = ui.grid.selection.clone();
    let bulk_bar = ui.bulk_bar.clone();
    let count_label = ui.bulk_count_label.clone();
    let pill = ui.pill.clone();

    // The pill appears whenever select mode is on (even at 0 selected, so the
    // user has an explicit "✕" to back out). The bottom action drawer only
    // reveals once ≥1 item is selected — actions on an empty selection are noise.
    let refresh = {
        let selection = selection.clone();
        let bulk_bar = bulk_bar.clone();
        let count_label = count_label.clone();
        let pill = pill.clone();
        let select_toggle = select_toggle.clone();
        Rc::new(move || {
            let active = select_toggle.is_active();
            let n = selection_count(&selection);
            pill.set_visible(active);
            bulk_bar.set_reveal_child(active && n > 0);
            count_label.set_label(&n.to_string());
        })
    };

    selection.connect_selection_changed({
        let refresh = refresh.clone();
        move |_, _, _| (*refresh)()
    });

    select_toggle.connect_toggled({
        let selection = selection.clone();
        let refresh = refresh.clone();
        move |toggle| {
            if !toggle.is_active() {
                selection.unselect_all();
            }
            (*refresh)();
        }
    });

    // Ctrl-hold transient checkboxes: pressing Ctrl reveals the selection UI;
    // releasing Ctrl without having selected anything dismisses it again.
    // Ctrl+click then commits a selection (handled separately in grid_view)
    // and the release no longer collapses select mode.
    let transient = Rc::new(Cell::new(false));
    let key_controller = gtk::EventControllerKey::new();
    key_controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    key_controller.connect_key_pressed({
        let select_toggle = select_toggle.clone();
        let transient = transient.clone();
        move |_, keyval, _, _| {
            if keyval == gtk::gdk::Key::Escape && select_toggle.is_active() {
                select_toggle.set_active(false);
                transient.set(false);
                return glib::Propagation::Stop;
            }
            if matches!(keyval, gtk::gdk::Key::Control_L | gtk::gdk::Key::Control_R)
                && !select_toggle.is_active()
            {
                select_toggle.set_active(true);
                transient.set(true);
            }
            glib::Propagation::Proceed
        }
    });
    key_controller.connect_key_released({
        let select_toggle = select_toggle.clone();
        let selection = selection.clone();
        let transient = transient.clone();
        move |_, keyval, _, _| {
            if !matches!(keyval, gtk::gdk::Key::Control_L | gtk::gdk::Key::Control_R)
                || !transient.get()
            {
                return;
            }
            if selection.selection().size() == 0 {
                select_toggle.set_active(false);
            }
            transient.set(false);
        }
    });
    ui.window.add_controller(key_controller);
}

pub(super) fn connect_bulk_actions(ui: Rc<LibraryWindowUi>) {
    // "✕" pill button: clear selection AND exit select mode.
    ui.pill_clear.connect_clicked(clone!(
        #[strong]
        ui,
        move |_| {
            ui.grid.selection.unselect_all();
            ui.select_toggle.set_active(false);
        }
    ));

    // Share → download the selected originals (Immich share-link API isn't
    // wired into mimick yet, so Share maps to the existing bulk-download flow).
    ui.bulk_download.connect_clicked(clone!(
        #[strong]
        ui,
        move |_| {
            let downloads: Vec<(String, String)> = collect_selected_assets(&ui)
                .into_iter()
                .filter(|(asset_id, _)| !asset_id.starts_with(LOCAL_ID_PREFIX))
                .collect();
            if !downloads.is_empty() {
                start_download_group(ui.clone(), downloads);
            }
        }
    ));

    ui.bulk_delete.connect_clicked(clone!(
        #[strong]
        ui,
        move |_| confirm_and_bulk_delete(ui.clone())
    ));

    ui.bulk_favorite.connect_clicked(clone!(
        #[strong]
        ui,
        move |_| bulk_set_favorite(ui.clone(), true)
    ));

    ui.bulk_archive.connect_clicked(clone!(
        #[strong]
        ui,
        move |_| bulk_set_archived(ui.clone(), true)
    ));

    ui.bulk_add_album.connect_clicked(clone!(
        #[strong]
        ui,
        move |_| bulk_add_to_album(ui.clone())
    ));

    ui.bulk_create_album.connect_clicked(clone!(
        #[strong]
        ui,
        move |_| bulk_create_album(ui.clone())
    ));
}

/// Remote asset ids in the current selection (local-only assets have no server
/// id and are skipped by every server-side bulk action).
fn selected_remote_ids(ui: &Rc<LibraryWindowUi>) -> Vec<String> {
    collect_selected_assets(ui)
        .into_iter()
        .filter(|(id, _)| !id.starts_with(LOCAL_ID_PREFIX))
        .map(|(id, _)| id)
        .collect()
}

/// Exit select mode, clear the selection, and refresh the grid. Mirrors the
/// post-`bulk_delete` cleanup so every action leaves the UI in the same state.
fn finish_bulk_action(ui: &Rc<LibraryWindowUi>) {
    ui.grid.selection.unselect_all();
    ui.select_toggle.set_active(false);
    super::refresh_library_after_mutation(ui.clone(), true);
}

/// Favorite (or unfavorite) every selected remote asset via a per-asset
/// `PUT /api/assets/{id}`. Immich has no bulk-favorite endpoint, so we loop.
fn bulk_set_favorite(ui: Rc<LibraryWindowUi>, favorite: bool) {
    let ids = selected_remote_ids(&ui);
    if ids.is_empty() {
        return;
    }
    glib::MainContext::default().spawn_local(async move {
        let mut failed = 0u32;
        for id in &ids {
            if let Err(err) = ui.ctx.api_client.set_asset_favorite(id, favorite).await {
                failed += 1;
                log::error!("Bulk favorite failed for {}: {}", id, err);
            }
        }
        if failed > 0 {
            log::warn!("Bulk favorite: {}/{} asset(s) failed", failed, ids.len());
        }
        finish_bulk_action(&ui);
    });
}

/// Archive (or unarchive) every selected remote asset via a per-asset
/// `PUT /api/assets/{id}` with `{"isArchived": ...}`. No bulk endpoint exists.
fn bulk_set_archived(ui: Rc<LibraryWindowUi>, archived: bool) {
    let ids = selected_remote_ids(&ui);
    if ids.is_empty() {
        return;
    }
    glib::MainContext::default().spawn_local(async move {
        let mut failed = 0u32;
        for id in &ids {
            if let Err(err) = ui.ctx.api_client.set_asset_archived(id, archived).await {
                failed += 1;
                log::error!("Bulk archive failed for {}: {}", id, err);
            }
        }
        if failed > 0 {
            log::warn!("Bulk archive: {}/{} asset(s) failed", failed, ids.len());
        }
        finish_bulk_action(&ui);
    });
}

/// Add the selected remote assets to an existing album, chosen from a picker.
/// Reuses the album-list dialog pattern from `staging_view`.
fn bulk_add_to_album(ui: Rc<LibraryWindowUi>) {
    let ids = selected_remote_ids(&ui);
    if ids.is_empty() {
        return;
    }
    let ui_c = ui.clone();
    glib::MainContext::default().spawn_local(async move {
        let albums = match ui_c.ctx.api_client.fetch_library_albums().await {
            Ok(a) => a,
            Err(err) => {
                log::error!("Add-to-album: could not load albums: {}", err);
                return;
            }
        };
        present_add_to_album_dialog(ui_c, albums, ids);
    });
}

/// Build + present the "choose an album" dialog for the bulk add-to-album flow.
fn present_add_to_album_dialog(
    ui: Rc<LibraryWindowUi>,
    albums: Vec<crate::api_client::LibraryAlbum>,
    ids: Vec<String>,
) {
    let dialog = libadwaita::AlertDialog::builder()
        .heading("Add to album")
        .body(format!("Add {} item(s) to which album?", ids.len()))
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
    dialog.add_response("add", "Add");
    dialog.set_response_appearance("add", libadwaita::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("add"));
    dialog.set_close_response("cancel");

    let albums_rc = Rc::new(albums);
    let ui_cb = ui.clone();
    dialog.connect_response(None, move |_dlg, response| {
        if response != "add" {
            return;
        }
        let Some(row) = listbox.selected_row() else {
            return;
        };
        let Some(album) = albums_rc.get(row.index() as usize) else {
            return;
        };
        let album_id = album.id.clone();
        let ui = ui_cb.clone();
        let ids = ids.clone();
        glib::MainContext::default().spawn_local(async move {
            let ok = ui
                .ctx
                .api_client
                .add_assets_to_album(&album_id, &ids)
                .await;
            if !ok {
                log::error!("Bulk add-to-album failed for album {}", album_id);
            }
            finish_bulk_action(&ui);
        });
    });
    dialog.present(Some(&ui.window));
}

/// Prompt for a new album name, create it, and add the selected assets to it.
fn bulk_create_album(ui: Rc<LibraryWindowUi>) {
    let ids = selected_remote_ids(&ui);
    if ids.is_empty() {
        return;
    }
    let dialog = libadwaita::AlertDialog::builder()
        .heading("Create new album")
        .body(format!("Create an album with the {} selected item(s).", ids.len()))
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

    let ui_cb = ui.clone();
    dialog.connect_response(None, move |dlg, response| {
        if response != "create" {
            return;
        }
        let name = entry.text().to_string();
        if name.trim().is_empty() {
            return;
        }
        let ui = ui_cb.clone();
        let ids = ids.clone();
        let name = name.trim().to_string();
        glib::MainContext::default().spawn_local(async move {
            match ui.ctx.api_client.create_album(&name).await {
                Ok(Some(album_id)) => {
                    let ok = ui.ctx.api_client.add_assets_to_album(&album_id, &ids).await;
                    if !ok {
                        log::error!("Create-album: add assets failed for {}", album_id);
                    }
                }
                Ok(None) => log::error!("Create-album: server returned no album id"),
                Err(err) => log::error!("Create-album failed: {}", err),
            }
            finish_bulk_action(&ui);
        });
        dlg.close();
    });
    dialog.present(Some(&ui.window));
}

pub(super) fn selection_count(selection: &gtk::MultiSelection) -> u32 {
    let bitset = selection.selection();
    bitset.size() as u32
}

pub(super) fn collect_selected_assets(ui: &Rc<LibraryWindowUi>) -> Vec<(String, String)> {
    let bitset = ui.grid.selection.selection();
    let mut out = Vec::new();
    let Some((mut iter, first)) = gtk::BitsetIter::init_first(&bitset) else {
        return out;
    };
    let mut pos = Some(first);
    while let Some(p) = pos {
        if let Some(item) = ui.grid.model.item(p).and_downcast::<AssetObject>() {
            out.push((
                item.property::<String>("id"),
                item.property::<String>("filename"),
            ));
        }
        pos = iter.next();
    }
    out
}

fn confirm_and_bulk_delete(ui: Rc<LibraryWindowUi>) {
    let assets = collect_selected_assets(&ui);
    let remote_ids: Vec<String> = assets
        .iter()
        .filter(|(id, _)| !id.starts_with(LOCAL_ID_PREFIX))
        .map(|(id, _)| id.clone())
        .collect();
    if remote_ids.is_empty() {
        return;
    }

    let dialog = libadwaita::AlertDialog::builder()
        .heading(format!("Move {} item(s) to trash?", remote_ids.len()))
        .body("Items can be restored from the Immich trash.")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", "Move to trash");
    dialog.set_response_appearance("delete", libadwaita::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let ui_for_choice = ui.clone();
    dialog.connect_response(None, move |dlg, response| {
        if response != "delete" {
            return;
        }
        let ui = ui_for_choice.clone();
        let ids = remote_ids.clone();
        glib::MainContext::default().spawn_local(async move {
            match ui.ctx.api_client.delete_assets(&ids).await {
                Ok(()) => {
                    ui.grid.selection.unselect_all();
                    ui.select_toggle.set_active(false);
                    super::refresh_library_after_mutation(ui.clone(), true);
                }
                Err(err) => {
                    log::error!("Bulk delete failed: {}", err);
                }
            }
        });
        dlg.close();
    });
    dialog.present(Some(&ui.window));
}
