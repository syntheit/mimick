//! Advanced metadata-filters dialog.

use std::rc::Rc;

use glib::clone;
use gtk::prelude::*;
use libadwaita::prelude::*;

use crate::api_client::MetadataSearchFilters;
use crate::library::state::LibrarySource;

use super::{LibraryWindowUi, load_source_page};

pub(super) fn connect_filters_button(ui: Rc<LibraryWindowUi>, filters_button: gtk::Button) {
    filters_button.connect_clicked(clone!(
        #[strong]
        ui,
        move |_| {
            present_advanced_filters_dialog(ui.clone());
        }
    ));
}

fn present_advanced_filters_dialog(ui: Rc<LibraryWindowUi>) {
    let dialog = libadwaita::Dialog::builder()
        .title("Advanced Filters")
        .content_width(520)
        .content_height(720)
        .width_request(360)
        .height_request(480)
        .build();
    let dialog_bp = libadwaita::Breakpoint::new(
        libadwaita::BreakpointCondition::parse("max-width: 400sp")
            .expect("valid breakpoint condition"),
    );
    dialog_bp.add_setter(&dialog, "content-width", Some(&(-1i32).to_value()));
    dialog_bp.add_setter(&dialog, "content-height", Some(&560i32.to_value()));
    dialog.add_breakpoint(dialog_bp);
    let toolbar = libadwaita::ToolbarView::builder().build();
    let header = libadwaita::HeaderBar::builder().build();
    toolbar.add_top_bar(&header);

    let page = libadwaita::PreferencesPage::new();

    let text_group = libadwaita::PreferencesGroup::builder()
        .title("Text")
        .description(
            "Description = user-set caption. OCR = text recognised inside images by Immich's ML \
             pipeline. All three are independent filter dimensions on /api/search/metadata.",
        )
        .build();
    let filename_row = libadwaita::EntryRow::builder()
        .title("Filename contains")
        .build();
    let description_row = libadwaita::EntryRow::builder()
        .title("Description contains")
        .build();
    let ocr_row = libadwaita::EntryRow::builder()
        .title("OCR text in image contains")
        .build();
    text_group.add(&filename_row);
    text_group.add(&description_row);
    text_group.add(&ocr_row);
    page.add(&text_group);

    // --- Type & flags ---
    let flags_group = libadwaita::PreferencesGroup::builder()
        .title("Type and flags")
        .build();
    let type_model = gtk::StringList::new(&["Any", "Image only", "Video only"]);
    let type_row = libadwaita::ComboRow::builder()
        .title("Asset type")
        .model(&type_model)
        .build();
    let favorite_row = libadwaita::SwitchRow::builder()
        .title("Favourites only")
        .build();
    let archived_row = libadwaita::SwitchRow::builder()
        .title("Archived only")
        .build();
    let motion_row = libadwaita::SwitchRow::builder()
        .title("Motion photos only")
        .build();
    let not_in_album_row = libadwaita::SwitchRow::builder()
        .title("Not in any album")
        .build();
    flags_group.add(&type_row);
    flags_group.add(&favorite_row);
    flags_group.add(&archived_row);
    flags_group.add(&motion_row);
    flags_group.add(&not_in_album_row);
    page.add(&flags_group);

    // --- Date range ---
    let date_group = libadwaita::PreferencesGroup::builder()
        .title("Date range")
        .description("ISO 8601 timestamps, e.g. 2024-01-15 or 2024-01-15T00:00:00Z")
        .build();
    let after_row = libadwaita::EntryRow::builder().title("Taken after").build();
    let before_row = libadwaita::EntryRow::builder()
        .title("Taken before")
        .build();
    date_group.add(&after_row);
    date_group.add(&before_row);
    page.add(&date_group);

    // --- Camera ---
    let camera_group = libadwaita::PreferencesGroup::builder()
        .title("Camera")
        .build();
    let make_row = libadwaita::EntryRow::builder().title("Make").build();
    let model_row = libadwaita::EntryRow::builder().title("Model").build();
    let lens_row = libadwaita::EntryRow::builder().title("Lens model").build();
    camera_group.add(&make_row);
    camera_group.add(&model_row);
    camera_group.add(&lens_row);
    page.add(&camera_group);

    // --- Location ---
    let loc_group = libadwaita::PreferencesGroup::builder()
        .title("Location")
        .build();
    let country_row = libadwaita::EntryRow::builder().title("Country").build();
    let state_row = libadwaita::EntryRow::builder()
        .title("State / region")
        .build();
    let city_row = libadwaita::EntryRow::builder().title("City").build();
    loc_group.add(&country_row);
    loc_group.add(&state_row);
    loc_group.add(&city_row);
    page.add(&loc_group);

    // --- Action buttons ---
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .margin_top(12)
        .margin_bottom(12)
        .margin_end(12)
        .build();
    let cancel_btn = gtk::Button::builder().label("Cancel").build();
    let apply_btn = gtk::Button::builder()
        .label("Apply")
        .css_classes(vec!["suggested-action".to_string()])
        .build();
    actions.append(&cancel_btn);
    actions.append(&apply_btn);

    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    outer.append(&page);
    outer.append(&actions);
    toolbar.set_content(Some(&outer));
    dialog.set_child(Some(&toolbar));

    cancel_btn.connect_clicked(clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));

    apply_btn.connect_clicked(clone!(
        #[strong]
        ui,
        #[weak]
        dialog,
        #[weak]
        filename_row,
        #[weak]
        description_row,
        #[weak]
        ocr_row,
        #[weak]
        type_row,
        #[weak]
        favorite_row,
        #[weak]
        archived_row,
        #[weak]
        motion_row,
        #[weak]
        not_in_album_row,
        #[weak]
        after_row,
        #[weak]
        before_row,
        #[weak]
        make_row,
        #[weak]
        model_row,
        #[weak]
        lens_row,
        #[weak]
        country_row,
        #[weak]
        state_row,
        #[weak]
        city_row,
        move |_| {
            let filters = MetadataSearchFilters {
                original_file_name: opt_string(&filename_row.text()),
                description: opt_string(&description_row.text()),
                ocr: opt_string(&ocr_row.text()),
                asset_type: match type_row.selected() {
                    1 => Some("IMAGE".into()),
                    2 => Some("VIDEO".into()),
                    _ => None,
                },
                taken_after: normalise_iso_date(&after_row.text()),
                taken_before: normalise_iso_date(&before_row.text()),
                make: opt_string(&make_row.text()),
                model: opt_string(&model_row.text()),
                lens_model: opt_string(&lens_row.text()),
                country: opt_string(&country_row.text()),
                state: opt_string(&state_row.text()),
                city: opt_string(&city_row.text()),
                is_favorite: opt_true(favorite_row.is_active()),
                is_archived: opt_true(archived_row.is_active()),
                is_motion: opt_true(motion_row.is_active()),
                is_not_in_album: opt_true(not_in_album_row.is_active()),
                with_exif: None,
                with_deleted: None,
                person_ids: None,
                tag_ids: None,
                order: None,
            };
            let request =
                ui.ctx
                    .library_state
                    .lock()
                    .switch_source(LibrarySource::AdvancedSearch {
                        filters: Box::new(filters),
                    });
            dialog.close();
            load_source_page(ui.clone(), request, false);
        }
    ));

    dialog.present(Some(&ui.window));
}

fn opt_string(text: &gtk::glib::GString) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn opt_true(active: bool) -> Option<bool> {
    if active { Some(true) } else { None }
}

fn normalise_iso_date(text: &gtk::glib::GString) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Already RFC3339? Pass through.
    if chrono::DateTime::parse_from_rfc3339(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }
    // Bare YYYY-MM-DD? Expand to midnight UTC.
    if chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").is_ok() {
        return Some(format!("{}T00:00:00.000Z", trimmed));
    }
    None
}
