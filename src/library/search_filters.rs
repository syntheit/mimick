//! Guided, grouped search-filters sheet (Phase 2 of the Search redesign).
//!
//! Replaces the old ~15-field free-text advanced-filters dialog
//! ([`super::filters::present_advanced_filters_dialog`]) with a Google-Photos
//! style bottom sheet: a short list of **grouped facet rows** (Who / Where /
//! When / What / More), each showing its current selection inline and pushing a
//! dedicated **picker** for the complex facets. No metadata typing — pick a
//! face, a city, a camera, or a date.
//!
//! # Layout
//!
//! One [`libadwaita::Dialog`] (auto-renders as a bottom sheet at phone width)
//! hosts a single [`libadwaita::NavigationView`]. The root page is the grouped
//! facet list; tapping a complex facet pushes an [`libadwaita::NavigationPage`]
//! picker *inside the same nav* (never a second stacked sheet). The filter model
//! is a shared `Rc<RefCell<MetadataSearchFilters>>` built up incrementally as
//! the user descends into pickers; **Apply** closes the sheet and drills into
//! `AdvancedSearch { filters }` on the Search tab.
//!
//! # Data sources per picker
//!
//! - **People** → `fetch_people` → writes `person_ids`.
//! - **Location** → `fetch_all_places` (cities) → writes `city`.
//! - **Camera** → `fetch_search_suggestions` (`camera-make`,
//!   `camera-model`) → writes `make` / `model`.
//! - **Date** → quick presets + two [`gtk::Calendar`] pickers, normalised via
//!   [`super::filters::normalise_iso_date`] → writes `taken_after` / `taken_before`.
//! - **Media type + flags** → inline in the root sheet → writes `asset_type`,
//!   `is_favorite`, `is_not_in_album`, `is_archived`, `is_motion`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gdk4::Texture;
use glib::Bytes;
use glib::clone;
use gtk::prelude::*;
use libadwaita::prelude::*;

use crate::api_client::MetadataSearchFilters;
use crate::app_context::AppContext;
use crate::library::state::LibrarySource;

use super::LibraryWindowUi;
use super::controls::tab_drill_in;
use super::filters::normalise_iso_date;

/// Shared, mutable filter model threaded through the sheet and its pickers.
type Filters = Rc<RefCell<MetadataSearchFilters>>;

/// Open the grouped Filters sheet for the Search tab. Seeds an empty
/// [`MetadataSearchFilters`]; the root page's rows push pickers that mutate it,
/// and **Apply** drills into the filtered results grid on the Search nav.
pub(super) fn present_search_filters_sheet(ui: Rc<LibraryWindowUi>) {
    let filters: Filters = Rc::new(RefCell::new(MetadataSearchFilters::default()));

    // Cache decoded person avatars for the People picker so re-rendering the
    // grid (search-as-you-type) never re-downloads. Scoped to this one sheet.
    let person_thumbs: Rc<RefCell<HashMap<String, Texture>>> =
        Rc::new(RefCell::new(HashMap::new()));

    let dialog = libadwaita::Dialog::builder()
        .title("Filters")
        // A comfortable phone-sheet size; AdwDialog auto-presents as a bottom
        // sheet on narrow windows and a centered dialog when wide.
        .content_width(420)
        .content_height(640)
        .build();

    let nav = libadwaita::NavigationView::new();
    let root = build_root_page(ui.clone(), dialog.clone(), nav.clone(), filters, person_thumbs);
    nav.add(&root);
    dialog.set_child(Some(&nav));

    dialog.present(Some(&ui.window));
}

/// Build the root facet-list page: grouped rows (Who / Where / When / What /
/// More) inside an `AdwToolbarView` with an Apply header action. Each complex
/// facet row is activatable and pushes its picker onto `nav`.
fn build_root_page(
    ui: Rc<LibraryWindowUi>,
    dialog: libadwaita::Dialog,
    nav: libadwaita::NavigationView,
    filters: Filters,
    person_thumbs: Rc<RefCell<HashMap<String, Texture>>>,
) -> libadwaita::NavigationPage {
    let toolbar = libadwaita::ToolbarView::new();
    let header = libadwaita::HeaderBar::builder()
        .show_end_title_buttons(true)
        .build();
    toolbar.add_top_bar(&header);

    let page = libadwaita::PreferencesPage::new();

    // --- Who → People ---
    let who_group = libadwaita::PreferencesGroup::builder().title("Who").build();
    let people_row = libadwaita::ActionRow::builder()
        .title("People")
        .subtitle("Any")
        .activatable(true)
        .build();
    people_row.add_suffix(&chevron());
    who_group.add(&people_row);
    page.add(&who_group);

    // --- Where → Location ---
    let where_group = libadwaita::PreferencesGroup::builder()
        .title("Where")
        .build();
    let location_row = libadwaita::ActionRow::builder()
        .title("Location")
        .subtitle("Any")
        .activatable(true)
        .build();
    location_row.add_suffix(&chevron());
    where_group.add(&location_row);
    page.add(&where_group);

    // --- When → Date ---
    let when_group = libadwaita::PreferencesGroup::builder()
        .title("When")
        .build();
    let date_row = libadwaita::ActionRow::builder()
        .title("Date")
        .subtitle("Any")
        .activatable(true)
        .build();
    date_row.add_suffix(&chevron());
    when_group.add(&date_row);
    page.add(&when_group);

    // --- What → Media type (inline segmented) + Camera (picker) ---
    let what_group = libadwaita::PreferencesGroup::builder()
        .title("What")
        .build();

    // Inline segmented media-type control: Any / Photos / Videos.
    let media_row = libadwaita::ActionRow::builder()
        .title("Media type")
        .activatable(false)
        .build();
    let segmented = build_media_segment(filters.clone());
    media_row.add_suffix(&segmented);
    what_group.add(&media_row);

    let camera_row = libadwaita::ActionRow::builder()
        .title("Camera")
        .subtitle("Any")
        .activatable(true)
        .build();
    camera_row.add_suffix(&chevron());
    what_group.add(&camera_row);
    page.add(&what_group);

    // --- More (collapsed) → switches + low-value free-text ---
    let more_group = libadwaita::PreferencesGroup::builder().build();
    let more = libadwaita::ExpanderRow::builder()
        .title("More")
        .subtitle("Favorites, albums, archived, motion, text")
        .build();

    let favorite_sw = switch_expander_row(&more, "Favorites only");
    let not_in_album_sw = switch_expander_row(&more, "Not in an album");
    let archived_sw = switch_expander_row(&more, "Archived only");
    let motion_sw = switch_expander_row(&more, "Motion photos only");

    // The only surviving free-text fields — low-value, buried under More.
    let filename_row = libadwaita::EntryRow::builder()
        .title("Filename contains")
        .build();
    let description_row = libadwaita::EntryRow::builder()
        .title("Description contains")
        .build();
    more.add_row(&filename_row);
    more.add_row(&description_row);

    // Reflect the switch/entry state into the shared model on change.
    wire_switch(&favorite_sw, filters.clone(), |f, on| {
        f.is_favorite = on.then_some(true)
    });
    wire_switch(&not_in_album_sw, filters.clone(), |f, on| {
        f.is_not_in_album = on.then_some(true)
    });
    wire_switch(&archived_sw, filters.clone(), |f, on| {
        f.is_archived = on.then_some(true)
    });
    wire_switch(&motion_sw, filters.clone(), |f, on| {
        f.is_motion = on.then_some(true)
    });
    wire_entry(&filename_row, filters.clone(), |f, v| {
        f.original_file_name = v
    });
    wire_entry(&description_row, filters.clone(), |f, v| f.description = v);

    more_group.add(&more);
    page.add(&more_group);

    // --- Apply action (primary; also mirrored as a full-width bottom button) ---
    let apply_btn = gtk::Button::builder()
        .label("Apply")
        .css_classes(["suggested-action"])
        .valign(gtk::Align::Center)
        .build();
    header.pack_end(&apply_btn);

    let bottom = gtk::Button::builder()
        .label("Show photos")
        .css_classes(["suggested-action", "pill"])
        .margin_start(16)
        .margin_end(16)
        .margin_top(8)
        .margin_bottom(12)
        .build();
    toolbar.add_bottom_bar(&bottom);

    let apply: Rc<dyn Fn()> = Rc::new(clone!(
        #[strong]
        ui,
        #[weak]
        dialog,
        #[strong]
        filters,
        move || {
            let f = filters.borrow().clone();
            dialog.close();
            tab_drill_in(
                ui.clone(),
                ui.search_tab.nav.clone(),
                "Filtered".to_string(),
                LibrarySource::AdvancedSearch {
                    filters: Box::new(f),
                },
            );
        }
    ));
    let apply_a = apply.clone();
    apply_btn.connect_clicked(move |_| apply_a());
    bottom.connect_clicked(move |_| apply());

    // --- Wire the complex-facet rows to push their pickers ---
    people_row.connect_activated(clone!(
        #[strong]
        ui,
        #[strong]
        nav,
        #[strong]
        filters,
        #[strong]
        person_thumbs,
        #[weak]
        people_row,
        move |_| {
            let picker = build_people_picker(
                ui.ctx.clone(),
                nav.clone(),
                filters.clone(),
                person_thumbs.clone(),
                people_row.clone(),
            );
            nav.push(&picker);
        }
    ));

    location_row.connect_activated(clone!(
        #[strong]
        ui,
        #[strong]
        nav,
        #[strong]
        filters,
        #[weak]
        location_row,
        move |_| {
            let picker =
                build_location_picker(ui.ctx.clone(), nav.clone(), filters.clone(), location_row.clone());
            nav.push(&picker);
        }
    ));

    date_row.connect_activated(clone!(
        #[strong]
        nav,
        #[strong]
        filters,
        #[weak]
        date_row,
        move |_| {
            let picker = build_date_picker(nav.clone(), filters.clone(), date_row.clone());
            nav.push(&picker);
        }
    ));

    camera_row.connect_activated(clone!(
        #[strong]
        ui,
        #[strong]
        nav,
        #[strong]
        filters,
        #[weak]
        camera_row,
        move |_| {
            let picker =
                build_camera_picker(ui.ctx.clone(), nav.clone(), filters.clone(), camera_row.clone());
            nav.push(&picker);
        }
    ));

    toolbar.set_content(Some(&page));

    libadwaita::NavigationPage::builder()
        .title("Filters")
        .child(&toolbar)
        .build()
}

/// A trailing chevron (`›`) suffix marking an activatable row that pushes a page.
fn chevron() -> gtk::Image {
    gtk::Image::builder()
        .icon_name("go-next-symbolic")
        .css_classes(["dim-label"])
        .build()
}

/// Inline segmented Any / Photos / Videos control writing `asset_type`.
/// Linked toggle buttons (`.linked`) give the standard segmented look without
/// AdwToggleGroup (which needs libadwaita 1.7 — the mimick pin is 1.6).
fn build_media_segment(filters: Filters) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .valign(gtk::Align::Center)
        .build();

    let any = gtk::ToggleButton::builder().label("Any").active(true).build();
    let photos = gtk::ToggleButton::builder().label("Photos").build();
    let videos = gtk::ToggleButton::builder().label("Videos").build();
    photos.set_group(Some(&any));
    videos.set_group(Some(&any));

    row.append(&any);
    row.append(&photos);
    row.append(&videos);

    // Each toggle writes `asset_type` when it becomes the active segment. Only
    // the newly-activated button fires with `is_active() == true`; the group
    // guarantees exactly one is active.
    any.connect_toggled(clone!(
        #[strong]
        filters,
        move |b| {
            if b.is_active() {
                filters.borrow_mut().asset_type = None;
            }
        }
    ));
    photos.connect_toggled(clone!(
        #[strong]
        filters,
        move |b| {
            if b.is_active() {
                filters.borrow_mut().asset_type = Some("IMAGE".to_string());
            }
        }
    ));
    videos.connect_toggled(clone!(
        #[strong]
        filters,
        move |b| {
            if b.is_active() {
                filters.borrow_mut().asset_type = Some("VIDEO".to_string());
            }
        }
    ));

    row
}

/// Add a labelled [`libadwaita::SwitchRow`] to an expander and return it.
fn switch_expander_row(
    expander: &libadwaita::ExpanderRow,
    title: &str,
) -> libadwaita::SwitchRow {
    let row = libadwaita::SwitchRow::builder().title(title).build();
    expander.add_row(&row);
    row
}

/// Wire a switch row so flipping it writes into the shared filter model.
fn wire_switch<F>(row: &libadwaita::SwitchRow, filters: Filters, apply: F)
where
    F: Fn(&mut MetadataSearchFilters, bool) + 'static,
{
    row.connect_active_notify(move |r| {
        apply(&mut filters.borrow_mut(), r.is_active());
    });
}

/// Wire a text entry row so edits write a trimmed `Option<String>` into the model.
fn wire_entry<F>(row: &libadwaita::EntryRow, filters: Filters, apply: F)
where
    F: Fn(&mut MetadataSearchFilters, Option<String>) + 'static,
{
    row.connect_changed(move |r| {
        let text = r.text();
        let trimmed = text.trim();
        let value = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        apply(&mut filters.borrow_mut(), value);
    });
}

// ---------------------------------------------------------------------------
// People picker
// ---------------------------------------------------------------------------

/// A searchable multi-select grid of person avatars. Writes `person_ids`; the
/// root `people_row` subtitle is updated to "N selected" on pop-back via the
/// live `selected` set. Backed by `fetch_people`.
fn build_people_picker(
    ctx: std::sync::Arc<AppContext>,
    nav: libadwaita::NavigationView,
    filters: Filters,
    person_thumbs: Rc<RefCell<HashMap<String, Texture>>>,
    summary_row: libadwaita::ActionRow,
) -> libadwaita::NavigationPage {
    let toolbar = libadwaita::ToolbarView::new();
    toolbar.add_top_bar(&libadwaita::HeaderBar::new());

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(12)
        .build();

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search people")
        .build();
    column.append(&search);

    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .column_spacing(8)
        .row_spacing(8)
        .max_children_per_line(4)
        .min_children_per_line(3)
        .build();

    let scroller = gtk::ScrolledWindow::builder()
        .child(&flow)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    column.append(&scroller);
    toolbar.set_content(Some(&column));

    // Seed `selected` from whatever the model already holds so reopening the
    // picker preserves prior choices.
    let selected: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(
        filters.borrow().person_ids.clone().unwrap_or_default(),
    ));
    // All fetched people, kept so the search box can re-render a filtered grid.
    let all_people: Rc<RefCell<Vec<crate::api_client::Person>>> =
        Rc::new(RefCell::new(Vec::new()));

    let render = {
        let ctx = ctx.clone();
        let flow = flow.clone();
        let selected = selected.clone();
        let all_people = all_people.clone();
        let person_thumbs = person_thumbs.clone();
        let filters = filters.clone();
        let summary_row = summary_row.clone();
        Rc::new(move |query: &str| {
            while let Some(child) = flow.first_child() {
                flow.remove(&child);
            }
            let q = query.to_ascii_lowercase();
            let people = all_people.borrow();
            for person in people.iter() {
                if person.name.is_empty() || person.is_hidden {
                    continue;
                }
                if !q.is_empty() && !person.name.to_ascii_lowercase().contains(&q) {
                    continue;
                }
                let tile = selectable_person_tile(
                    ctx.clone(),
                    person,
                    selected.clone(),
                    filters.clone(),
                    person_thumbs.clone(),
                    summary_row.clone(),
                );
                flow.append(&tile);
            }
        })
    };

    search.connect_search_changed(clone!(
        #[strong]
        render,
        move |e| render(e.text().as_str())
    ));

    // Fetch people, then render. Guard against a closed sheet: if the nav's
    // toplevel window is gone the async result is simply dropped.
    let nav_weak = nav.downgrade();
    glib::MainContext::default().spawn_local(clone!(
        #[strong]
        ctx,
        #[strong]
        all_people,
        #[strong]
        render,
        async move {
            let people = ctx.api_client.fetch_people(false).await.unwrap_or_default();
            if nav_weak.upgrade().is_none() {
                return;
            }
            *all_people.borrow_mut() = people;
            render("");
        }
    ));

    libadwaita::NavigationPage::builder()
        .title("People")
        .child(&toolbar)
        .build()
}

/// One selectable avatar: the person's face + name with a checkmark overlay that
/// toggles membership in `selected`, writing `person_ids` and updating the
/// summary row subtitle immediately.
fn selectable_person_tile(
    ctx: std::sync::Arc<AppContext>,
    person: &crate::api_client::Person,
    selected: Rc<RefCell<Vec<String>>>,
    filters: Filters,
    person_thumbs: Rc<RefCell<HashMap<String, Texture>>>,
    summary_row: libadwaita::ActionRow,
) -> gtk::Widget {
    let avatar = gtk::Picture::builder()
        .width_request(84)
        .height_request(84)
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Cover)
        .css_classes(["mimick-person-avatar"])
        .build();

    // A check badge overlaid on the avatar; visible only while selected.
    let check = gtk::Image::builder()
        .icon_name("emblem-ok-symbolic")
        .halign(gtk::Align::End)
        .valign(gtk::Align::End)
        .css_classes(["mimick-people-check"])
        .build();
    let is_selected = selected.borrow().contains(&person.id);
    check.set_visible(is_selected);

    let overlay = gtk::Overlay::builder().child(&avatar).build();
    overlay.add_overlay(&check);

    let label = gtk::Label::builder()
        .label(&person.name)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(10)
        .build();

    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .halign(gtk::Align::Center)
        .build();
    inner.append(&overlay);
    inner.append(&label);

    let button = gtk::Button::builder()
        .child(&inner)
        .css_classes(["flat"])
        .build();

    let id = person.id.clone();
    button.connect_clicked(clone!(
        #[strong]
        selected,
        #[strong]
        filters,
        #[weak]
        check,
        #[weak]
        summary_row,
        move |_| {
            let mut sel = selected.borrow_mut();
            if let Some(pos) = sel.iter().position(|p| p == &id) {
                sel.remove(pos);
                check.set_visible(false);
            } else {
                sel.push(id.clone());
                check.set_visible(true);
            }
            let ids = sel.clone();
            let count = ids.len();
            drop(sel);
            filters.borrow_mut().person_ids = if ids.is_empty() { None } else { Some(ids) };
            summary_row.set_subtitle(&people_summary(count));
        }
    ));

    spawn_person_avatar(ctx, person.id.clone(), avatar, person_thumbs);
    button.upcast::<gtk::Widget>()
}

/// Subtitle text for the People row given a selected count.
fn people_summary(n: usize) -> String {
    match n {
        0 => "Any".to_string(),
        1 => "1 selected".to_string(),
        n => format!("{n} selected"),
    }
}

/// Load a person's face thumbnail into `picture`, caching the decoded texture.
/// A local reimplementation of `explore_view`'s private helper so the picker is
/// self-contained (the source function is not exported).
fn spawn_person_avatar(
    ctx: std::sync::Arc<AppContext>,
    person_id: String,
    picture: gtk::Picture,
    thumb_cache: Rc<RefCell<HashMap<String, Texture>>>,
) {
    if let Some(texture) = thumb_cache.borrow().get(&person_id) {
        picture.set_paintable(Some(texture));
        return;
    }
    glib::MainContext::default().spawn_local(async move {
        if let Some(texture) = thumb_cache.borrow().get(&person_id) {
            picture.set_paintable(Some(texture));
            return;
        }
        let bytes = match ctx.api_client.fetch_person_thumbnail(&person_id).await {
            Ok(b) => b,
            Err(_) => return,
        };
        let texture = tokio::task::spawn_blocking(move || -> Option<Texture> {
            Texture::from_bytes(&Bytes::from(&bytes[..])).ok()
        })
        .await
        .ok()
        .flatten();
        if let Some(texture) = texture {
            thumb_cache.borrow_mut().insert(person_id, texture.clone());
            picture.set_paintable(Some(&texture));
        }
    });
}

// ---------------------------------------------------------------------------
// Location picker
// ---------------------------------------------------------------------------

/// A searchable single-select list of cities from `fetch_all_places`. Tapping
/// a city writes `city` and updates the summary row; "Any location" clears it.
fn build_location_picker(
    ctx: std::sync::Arc<AppContext>,
    nav: libadwaita::NavigationView,
    filters: Filters,
    summary_row: libadwaita::ActionRow,
) -> libadwaita::NavigationPage {
    let toolbar = libadwaita::ToolbarView::new();
    toolbar.add_top_bar(&libadwaita::HeaderBar::new());

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(12)
        .build();

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search cities")
        .build();
    column.append(&search);

    let listbox = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&listbox)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    column.append(&scroller);
    toolbar.set_content(Some(&column));

    let cities: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let current = filters.borrow().city.clone();

    let render = {
        let listbox = listbox.clone();
        let cities = cities.clone();
        let filters = filters.clone();
        let summary_row = summary_row.clone();
        let nav = nav.clone();
        let selected = Rc::new(RefCell::new(current));
        Rc::new(move |query: &str| {
            while let Some(child) = listbox.first_child() {
                listbox.remove(&child);
            }
            // "Any location" clear row first.
            let any = libadwaita::ActionRow::builder()
                .title("Any location")
                .activatable(true)
                .build();
            if selected.borrow().is_none() {
                any.add_suffix(&checkmark());
            }
            listbox.append(&any);
            any.connect_activated(clone!(
                #[strong]
                filters,
                #[strong]
                summary_row,
                #[weak]
                nav,
                move |_| {
                    filters.borrow_mut().city = None;
                    summary_row.set_subtitle("Any");
                    nav.pop();
                }
            ));

            let q = query.to_ascii_lowercase();
            let list = cities.borrow();
            let sel = selected.borrow().clone();
            for city in list.iter() {
                if !q.is_empty() && !city.to_ascii_lowercase().contains(&q) {
                    continue;
                }
                let row = libadwaita::ActionRow::builder()
                    .title(city)
                    .activatable(true)
                    .build();
                if sel.as_deref() == Some(city.as_str()) {
                    row.add_suffix(&checkmark());
                }
                let city_owned = city.clone();
                row.connect_activated(clone!(
                    #[strong]
                    filters,
                    #[strong]
                    summary_row,
                    #[weak]
                    nav,
                    move |_| {
                        filters.borrow_mut().city = Some(city_owned.clone());
                        summary_row.set_subtitle(&city_owned);
                        nav.pop();
                    }
                ));
                listbox.append(&row);
            }
        })
    };

    search.connect_search_changed(clone!(
        #[strong]
        render,
        move |e| render(e.text().as_str())
    ));

    let nav_weak = nav.downgrade();
    glib::MainContext::default().spawn_local(clone!(
        #[strong]
        ctx,
        #[strong]
        cities,
        #[strong]
        render,
        async move {
            let places = ctx.api_client.fetch_all_places().await.unwrap_or_default();
            if nav_weak.upgrade().is_none() {
                return;
            }
            *cities.borrow_mut() = places.into_iter().map(|p| p.city).collect();
            render("");
        }
    ));

    libadwaita::NavigationPage::builder()
        .title("Location")
        .child(&toolbar)
        .build()
}

/// A selection checkmark suffix for the currently-chosen list row.
fn checkmark() -> gtk::Image {
    gtk::Image::builder()
        .icon_name("emblem-ok-symbolic")
        .css_classes(["accent"])
        .build()
}

// ---------------------------------------------------------------------------
// Date picker
// ---------------------------------------------------------------------------

/// Quick presets + custom range (two calendars). Writes normalised
/// `taken_after` / `taken_before`; no ISO typing.
fn build_date_picker(
    nav: libadwaita::NavigationView,
    filters: Filters,
    summary_row: libadwaita::ActionRow,
) -> libadwaita::NavigationPage {
    let toolbar = libadwaita::ToolbarView::new();
    toolbar.add_top_bar(&libadwaita::HeaderBar::new());

    let page = libadwaita::PreferencesPage::new();

    let presets = libadwaita::PreferencesGroup::builder()
        .title("Quick ranges")
        .build();

    // Build the preset list dynamically: Any, This year, Last 12 months, and a
    // few recent calendar years.
    let today = glib::DateTime::now_local().unwrap_or_else(|_| {
        glib::DateTime::from_unix_utc(0).expect("epoch DateTime")
    });
    let this_year = today.year();

    // Last 12 months: after = today minus one year.
    let year_ago = today.add_years(-1).unwrap_or_else(|_| today.clone());

    // (label, after, before) — bounds are RFC3339 strings or None.
    let mut preset_rows: Vec<(String, Option<String>, Option<String>)> = vec![
        ("Any date".to_string(), None, None),
        (
            "This year".to_string(),
            Some(iso_day_start(this_year, 1, 1)),
            None,
        ),
        (
            "Last 12 months".to_string(),
            Some(iso_from_datetime_start(&year_ago)),
            None,
        ),
    ];
    for y in (this_year - 3..this_year).rev() {
        preset_rows.push((
            format!("{y}"),
            Some(iso_day_start(y, 1, 1)),
            Some(iso_day_end(y, 12, 31)),
        ));
    }

    for (label, after, before) in preset_rows {
        let row = libadwaita::ActionRow::builder()
            .title(&label)
            .activatable(true)
            .build();
        row.connect_activated(clone!(
            #[strong]
            filters,
            #[strong]
            summary_row,
            #[weak]
            nav,
            move |_| {
                {
                    let mut f = filters.borrow_mut();
                    f.taken_after = after.clone();
                    f.taken_before = before.clone();
                }
                summary_row.set_subtitle(&label);
                nav.pop();
            }
        ));
        presets.add(&row);
    }
    page.add(&presets);

    // --- Custom range: two calendars (collapsed in expanders) + an apply ---
    let custom = libadwaita::PreferencesGroup::builder()
        .title("Custom range")
        .build();

    let after_cal = gtk::Calendar::new();
    let before_cal = gtk::Calendar::new();

    // AdwExpanderRow.add_row reveals any child widget under the row; a bare
    // Calendar renders cleanly there and keeps the two pickers collapsed until
    // the user wants them.
    let after_expander = libadwaita::ExpanderRow::builder().title("From").build();
    after_expander.add_row(&after_cal);
    let before_expander = libadwaita::ExpanderRow::builder().title("To").build();
    before_expander.add_row(&before_cal);

    // A plain activatable ActionRow as the "apply this custom range" button —
    // avoids AdwButtonRow (whose bindings are gated on a gtk feature the pin may
    // not enable) while still reading as a tappable row inside the group.
    let apply_range = libadwaita::ActionRow::builder()
        .title("Use custom range")
        .activatable(true)
        .css_classes(["suggested-action"])
        .build();
    apply_range.add_prefix(
        &gtk::Image::from_icon_name("x-office-calendar-symbolic"),
    );

    custom.add(&after_expander);
    custom.add(&before_expander);
    custom.add(&apply_range);
    page.add(&custom);

    apply_range.connect_activated(clone!(
        #[strong]
        filters,
        #[strong]
        summary_row,
        #[weak]
        nav,
        #[weak]
        after_cal,
        #[weak]
        before_cal,
        move |_| {
            let after = calendar_iso_start(&after_cal);
            let before = calendar_iso_end(&before_cal);
            {
                let mut f = filters.borrow_mut();
                f.taken_after = after.clone();
                f.taken_before = before.clone();
            }
            summary_row.set_subtitle(&format!(
                "{} – {}",
                calendar_label(&after_cal),
                calendar_label(&before_cal)
            ));
            nav.pop();
        }
    ));

    toolbar.set_content(Some(&page));
    libadwaita::NavigationPage::builder()
        .title("Date")
        .child(&toolbar)
        .build()
}

/// `YYYY-MM-DD` at midnight UTC → RFC3339 via `normalise_iso_date`.
fn iso_day_start(year: i32, month: u32, day: u32) -> String {
    normalise_iso_date(&format!("{year:04}-{month:02}-{day:02}"))
        .unwrap_or_else(|| format!("{year:04}-{month:02}-{day:02}T00:00:00.000Z"))
}

/// `YYYY-MM-DD` at the end of the day (inclusive upper bound).
fn iso_day_end(year: i32, month: u32, day: u32) -> String {
    format!("{year:04}-{month:02}-{day:02}T23:59:59.999Z")
}

/// Start-of-day ISO for a `glib::DateTime` (used by "Last 12 months").
fn iso_from_datetime_start(dt: &glib::DateTime) -> String {
    iso_day_start(dt.year(), dt.month() as u32, dt.day_of_month() as u32)
}

/// Selected calendar date → start-of-day RFC3339.
fn calendar_iso_start(cal: &gtk::Calendar) -> Option<String> {
    let d = cal.date();
    Some(iso_day_start(
        d.year(),
        d.month() as u32,
        d.day_of_month() as u32,
    ))
}

/// Selected calendar date → end-of-day RFC3339 (inclusive `before`).
fn calendar_iso_end(cal: &gtk::Calendar) -> Option<String> {
    let d = cal.date();
    Some(iso_day_end(
        d.year(),
        d.month() as u32,
        d.day_of_month() as u32,
    ))
}

/// Short `YYYY-MM-DD` label for a calendar's selected date.
fn calendar_label(cal: &gtk::Calendar) -> String {
    let d = cal.date();
    format!(
        "{:04}-{:02}-{:02}",
        d.year(),
        d.month(),
        d.day_of_month()
    )
}

// ---------------------------------------------------------------------------
// Camera picker
// ---------------------------------------------------------------------------

/// Two single-select lists (makes, models) from
/// `fetch_search_suggestions`. Tapping a make writes `make`;
/// tapping a model writes `model`. "Any camera" clears both.
fn build_camera_picker(
    ctx: std::sync::Arc<AppContext>,
    nav: libadwaita::NavigationView,
    filters: Filters,
    summary_row: libadwaita::ActionRow,
) -> libadwaita::NavigationPage {
    let toolbar = libadwaita::ToolbarView::new();
    toolbar.add_top_bar(&libadwaita::HeaderBar::new());

    let page = libadwaita::PreferencesPage::new();

    // Clear row.
    let clear_group = libadwaita::PreferencesGroup::new();
    let any_row = libadwaita::ActionRow::builder()
        .title("Any camera")
        .activatable(true)
        .build();
    any_row.connect_activated(clone!(
        #[strong]
        filters,
        #[strong]
        summary_row,
        #[weak]
        nav,
        move |_| {
            {
                let mut f = filters.borrow_mut();
                f.make = None;
                f.model = None;
            }
            summary_row.set_subtitle("Any");
            nav.pop();
        }
    ));
    clear_group.add(&any_row);
    page.add(&clear_group);

    let make_group = libadwaita::PreferencesGroup::builder()
        .title("Make")
        .build();
    page.add(&make_group);
    let model_group = libadwaita::PreferencesGroup::builder()
        .title("Model")
        .build();
    page.add(&model_group);

    // Populate makes.
    populate_suggestion_group(
        ctx.clone(),
        nav.clone(),
        "camera-make",
        make_group.clone(),
        filters.clone(),
        summary_row.clone(),
        |f, value| f.make = Some(value),
    );
    // Populate models.
    populate_suggestion_group(
        ctx.clone(),
        nav.clone(),
        "camera-model",
        model_group.clone(),
        filters.clone(),
        summary_row.clone(),
        |f, value| f.model = Some(value),
    );

    toolbar.set_content(Some(&page));
    libadwaita::NavigationPage::builder()
        .title("Camera")
        .child(&toolbar)
        .build()
}

/// Fetch `suggestion_type` values and fill `group` with a tappable row per value.
/// `apply` records the pick into the model; the summary row reflects it.
fn populate_suggestion_group<F>(
    ctx: std::sync::Arc<AppContext>,
    nav: libadwaita::NavigationView,
    suggestion_type: &'static str,
    group: libadwaita::PreferencesGroup,
    filters: Filters,
    summary_row: libadwaita::ActionRow,
    apply: F,
) where
    F: Fn(&mut MetadataSearchFilters, String) + 'static,
{
    let apply = Rc::new(apply);
    let nav_weak = nav.downgrade();
    glib::MainContext::default().spawn_local(clone!(
        #[strong]
        ctx,
        #[strong]
        group,
        #[strong]
        filters,
        #[strong]
        summary_row,
        async move {
            let values = ctx
                .api_client
                .fetch_search_suggestions(suggestion_type)
                .await
                .unwrap_or_default();
            let Some(nav) = nav_weak.upgrade() else {
                return;
            };
            if values.is_empty() {
                group.set_visible(false);
                return;
            }
            for value in values {
                let row = libadwaita::ActionRow::builder()
                    .title(&value)
                    .activatable(true)
                    .build();
                let apply = apply.clone();
                let value_owned = value.clone();
                row.connect_activated(clone!(
                    #[strong]
                    filters,
                    #[strong]
                    summary_row,
                    #[weak]
                    nav,
                    move |_| {
                        apply(&mut filters.borrow_mut(), value_owned.clone());
                        summary_row.set_subtitle(&value_owned);
                        nav.pop();
                    }
                ));
                group.add(&row);
            }
        }
    ));
}
