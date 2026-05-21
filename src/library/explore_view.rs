//! Sectioned landing page mirroring Immich web's Explore tab.
//!
//! Three rows: People (round avatars), Places (city tiles), Things (tag
//! tiles). Tile clicks invoke caller-provided closures so dispatch lives in
//! `mod.rs` and this module stays UI-only.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gdk4::Texture;
use glib::Bytes;
use gtk::prelude::*;

use crate::api_client::{ExploreSection, Person, PlaceItem, ThumbnailSize};
use crate::app_context::AppContext;

type ExploreClick = Rc<dyn Fn(&str, String)>;
type PersonClick = Rc<dyn Fn(String, String)>;

/// Contains references to individual grid widgets of the explore tab dashboard display.
pub struct ExploreViewParts {
    pub root: gtk::ScrolledWindow,
    pub populated: Rc<Cell<bool>>,
    people_row: gtk::Box,
    places_grid: gtk::FlowBox,
    things_grid: gtk::FlowBox,
    people_section: gtk::Box,
    places_section: gtk::Box,
    things_section: gtk::Box,
    pub people_filter_button: gtk::MenuButton,
    cached_people: Rc<RefCell<Vec<Person>>>,
    cached_people_click: Rc<RefCell<Option<PersonClick>>>,
}

/// Construct the hierarchical panels and containers for the explore dashboard view.
pub fn build_explore_view() -> ExploreViewParts {
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();

    let (people_section, people_row, people_filter_button) = build_people_section();
    let (places_section, places_grid) = build_tile_section("Places");
    let (things_section, things_grid) = build_tile_section("Things");

    outer.append(&people_section);
    outer.append(&places_section);
    outer.append(&things_section);

    let root = gtk::ScrolledWindow::builder()
        .child(&outer)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .hexpand(true)
        .build();

    ExploreViewParts {
        root,
        populated: Rc::new(Cell::new(false)),
        people_row,
        places_grid,
        things_grid,
        people_section,
        places_section,
        things_section,
        people_filter_button,
        cached_people: Rc::new(RefCell::new(Vec::new())),
        cached_people_click: Rc::new(RefCell::new(None)),
    }
}

/// Build a horizontal scrolled gallery row dedicated to recognized people circles.
fn build_people_section() -> (gtk::Box, gtk::Box, gtk::MenuButton) {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .visible(false)
        .build();

    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let title = heading("People");
    title.set_hexpand(true);
    header.append(&title);
    let filter_button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Filter people")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    header.append(&filter_button);
    section.append(&header);

    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&row)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .height_request(140)
        .build();
    section.append(&scroller);
    (section, row, filter_button)
}

/// Build a flow grid section mapping image tiles for Places or Things category.
fn build_tile_section(title: &str) -> (gtk::Box, gtk::FlowBox) {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .visible(false)
        .build();
    section.append(&heading(title));
    let grid = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .row_spacing(8)
        .column_spacing(8)
        .min_children_per_line(2)
        .max_children_per_line(6)
        .homogeneous(true)
        .build();
    section.append(&grid);
    (section, grid)
}

/// Helper to create a styled section heading label.
fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .css_classes(vec!["title-2".to_string()])
        .build()
}

/// Populate the round avatar buttons for recognized people in the dashboard section.
///
/// Stores the full fetched list so face-visibility toggles can re-filter without
/// re-querying the server. Always call with `include_hidden=true` upstream so the
/// `Show hidden` toggle can apply locally.
pub fn populate_people<F>(
    parts: &ExploreViewParts,
    ctx: Arc<AppContext>,
    people: Vec<Person>,
    on_click: F,
) where
    F: Fn(String, String) + 'static,
{
    *parts.cached_people.borrow_mut() = people;
    *parts.cached_people_click.borrow_mut() = Some(Rc::new(on_click));
    render_people(parts, ctx);
}

fn render_people(parts: &ExploreViewParts, ctx: Arc<AppContext>) {
    while let Some(child) = parts.people_row.first_child() {
        parts.people_row.remove(&child);
    }
    let (show_unnamed, show_hidden) = {
        let cfg = ctx.config.read();
        (cfg.data.show_unnamed_faces, cfg.data.show_hidden_faces)
    };
    let cached = parts.cached_people.borrow();
    let filtered: Vec<&Person> = cached
        .iter()
        .filter(|p| show_hidden || !p.is_hidden)
        .filter(|p| show_unnamed || !p.name.is_empty())
        .collect();
    parts.people_section.set_visible(!filtered.is_empty());
    let on_click = parts.cached_people_click.borrow().clone();
    let Some(on_click) = on_click else {
        return;
    };
    for person in filtered.into_iter().take(40) {
        let tile = person_tile(ctx.clone(), person, on_click.clone());
        parts.people_row.append(&tile);
    }
}

/// Wire the filter MenuButton popover. Called once after the explore view is built.
/// `on_change` is invoked when a toggle flips so the caller can re-fetch with a
/// different `include_hidden` flag if needed.
pub fn wire_people_filter<F>(parts: &ExploreViewParts, ctx: Arc<AppContext>, on_change: F)
where
    F: Fn() + 'static,
{
    let popover = gtk::Popover::builder().build();
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();

    let (show_unnamed, show_hidden) = {
        let cfg = ctx.config.read();
        (cfg.data.show_unnamed_faces, cfg.data.show_hidden_faces)
    };

    let unnamed_check = gtk::CheckButton::builder()
        .label("Show unnamed")
        .active(show_unnamed)
        .build();
    let hidden_check = gtk::CheckButton::builder()
        .label("Show hidden")
        .active(show_hidden)
        .build();
    body.append(&unnamed_check);
    body.append(&hidden_check);
    popover.set_child(Some(&body));
    parts.people_filter_button.set_popover(Some(&popover));

    let on_change = Rc::new(on_change);

    let ctx_a = ctx.clone();
    let parts_a = clone_parts_handles(parts);
    let on_change_a = on_change.clone();
    unnamed_check.connect_toggled(move |btn| {
        {
            let mut cfg = ctx_a.config.write();
            cfg.data.show_unnamed_faces = btn.is_active();
            if !cfg.save() {
                log::error!("Failed to save config after toggling show_unnamed_faces");
            }
        }
        render_people(&parts_a, ctx_a.clone());
        on_change_a();
    });

    let ctx_b = ctx.clone();
    let parts_b = clone_parts_handles(parts);
    let on_change_b = on_change.clone();
    hidden_check.connect_toggled(move |btn| {
        {
            let mut cfg = ctx_b.config.write();
            cfg.data.show_hidden_faces = btn.is_active();
            if !cfg.save() {
                log::error!("Failed to save config after toggling show_hidden_faces");
            }
        }
        render_people(&parts_b, ctx_b.clone());
        on_change_b();
    });
}

/// Build a lightweight `ExploreViewParts` snapshot containing only the widget/handle
/// references render_people needs, sharing the same `Rc` data with the original.
fn clone_parts_handles(parts: &ExploreViewParts) -> ExploreViewParts {
    ExploreViewParts {
        root: parts.root.clone(),
        populated: parts.populated.clone(),
        people_row: parts.people_row.clone(),
        places_grid: parts.places_grid.clone(),
        things_grid: parts.things_grid.clone(),
        people_section: parts.people_section.clone(),
        places_section: parts.places_section.clone(),
        things_section: parts.things_section.clone(),
        people_filter_button: parts.people_filter_button.clone(),
        cached_people: parts.cached_people.clone(),
        cached_people_click: parts.cached_people_click.clone(),
    }
}

/// Populate the city tiles representing locations in the places dashboard section.
pub fn populate_places<F>(
    parts: &ExploreViewParts,
    ctx: Arc<AppContext>,
    places: Vec<PlaceItem>,
    on_click: F,
) where
    F: Fn(&str, String) + 'static,
{
    while let Some(child) = parts.places_grid.first_child() {
        parts.places_grid.remove(&child);
    }
    parts.places_section.set_visible(!places.is_empty());
    let on_click = Rc::new(on_click);
    for place in places {
        let tile = explore_tile(
            ctx.clone(),
            "place",
            &place.city,
            &place.asset_id,
            on_click.clone(),
        );
        parts.places_grid.append(&tile);
    }
}

/// Populate general objects, scenes, and tags in the things dashboard section.
pub fn populate_explore<F>(
    parts: &ExploreViewParts,
    ctx: Arc<AppContext>,
    sections: Vec<ExploreSection>,
    on_click: F,
) where
    F: Fn(&str, String) + 'static,
{
    while let Some(child) = parts.things_grid.first_child() {
        parts.things_grid.remove(&child);
    }
    let mut had_things = false;

    let on_click = Rc::new(on_click);
    for section in sections {
        if section.field_name.contains("city") {
            // Places are populated separately via populate_places.
            continue;
        }
        had_things = true;
        for item in section.items.into_iter().take(24) {
            let tile = explore_tile(
                ctx.clone(),
                "thing",
                &item.value,
                &item.data.id,
                on_click.clone(),
            );
            parts.things_grid.append(&tile);
        }
    }

    parts.things_section.set_visible(had_things);
}

/// Construct an individual circular avatar widget representing a recognized person face.
fn person_tile(
    ctx: Arc<AppContext>,
    person: &Person,
    on_click: Rc<dyn Fn(String, String)>,
) -> gtk::Button {
    let avatar = gtk::Picture::builder()
        .width_request(96)
        .height_request(96)
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Cover)
        .css_classes(vec!["mimick-person-avatar".to_string()])
        .build();
    let label = gtk::Label::builder()
        .label(if person.name.is_empty() {
            "Unnamed"
        } else {
            &person.name
        })
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(12)
        .build();
    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .halign(gtk::Align::Center)
        .build();
    inner.append(&avatar);
    inner.append(&label);

    let button = gtk::Button::builder()
        .child(&inner)
        .css_classes(vec!["flat".to_string()])
        .build();

    let id = person.id.clone();
    let name = if person.name.is_empty() {
        "Unnamed".to_string()
    } else {
        person.name.clone()
    };
    button.connect_clicked(move |_| on_click(id.clone(), name.clone()));

    spawn_person_thumbnail(ctx, person.id.clone(), avatar);
    button
}

/// Construct a rectangular tile representation for a specific explore category node.
fn explore_tile(
    ctx: Arc<AppContext>,
    kind: &'static str,
    value: &str,
    asset_id: &str,
    on_click: ExploreClick,
) -> gtk::Button {
    let picture = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Cover)
        .height_request(100)
        .hexpand(true)
        .vexpand(false)
        .css_classes(vec!["mimick-explore-tile".to_string()])
        .build();
    let label = gtk::Label::builder()
        .label(value)
        .xalign(0.0)
        .width_chars(1)
        .max_width_chars(1)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(vec!["caption-heading".to_string()])
        .build();
    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .hexpand(true)
        .build();
    inner.append(&picture);
    inner.append(&label);

    let button = gtk::Button::builder()
        .child(&inner)
        .css_classes(vec!["flat".to_string()])
        .hexpand(true)
        .halign(gtk::Align::Fill)
        .build();

    let value_owned = value.to_string();
    button.connect_clicked(move |_| on_click(kind, value_owned.clone()));

    spawn_asset_thumbnail(ctx, asset_id.to_string(), picture);
    button
}

/// Helper to asynchronously request and bind an asset cover art thumbnail image.
fn spawn_asset_thumbnail(ctx: Arc<AppContext>, asset_id: String, picture: gtk::Picture) {
    if let Some(texture) = ctx
        .thumbnail_cache
        .get_cached(&asset_id, ThumbnailSize::Thumbnail)
    {
        picture.set_paintable(Some(&texture));
        return;
    }
    glib::timeout_add_local_once(std::time::Duration::from_millis(120), move || {
        glib::MainContext::default().spawn_local(async move {
            if let Ok(texture) = ctx
                .thumbnail_cache
                .load_thumbnail(&asset_id, ThumbnailSize::Thumbnail)
                .await
            {
                picture.set_paintable(Some(&texture));
            }
        });
    });
}

/// Helper to asynchronously request and render a round avatar person face thumbnail.
fn spawn_person_thumbnail(ctx: Arc<AppContext>, person_id: String, picture: gtk::Picture) {
    glib::timeout_add_local_once(std::time::Duration::from_millis(120), move || {
        glib::MainContext::default().spawn_local(async move {
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
                picture.set_paintable(Some(&texture));
            }
        });
    });
}
