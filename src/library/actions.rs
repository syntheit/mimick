//! Selection-mode wiring and bulk batch operations.

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

    let refresh = {
        let selection = selection.clone();
        let bulk_bar = bulk_bar.clone();
        let count_label = count_label.clone();
        let select_toggle = select_toggle.clone();
        Rc::new(move || {
            let n = selection_count(&selection);
            bulk_bar.set_reveal_child(select_toggle.is_active() && n > 0);
            count_label.set_label(&format!("{} selected", n));
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

pub(super) fn connect_bulk_actions(
    ui: Rc<LibraryWindowUi>,
    delete_btn: gtk::Button,
    download_btn: gtk::Button,
    clear_btn: gtk::Button,
) {
    clear_btn.connect_clicked(clone!(
        #[strong]
        ui,
        move |_| {
            ui.grid.selection.unselect_all();
            ui.select_toggle.set_active(false);
        }
    ));

    download_btn.connect_clicked(clone!(
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

    delete_btn.connect_clicked(clone!(
        #[strong]
        ui,
        move |_| {
            confirm_and_bulk_delete(ui.clone());
        }
    ));
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
