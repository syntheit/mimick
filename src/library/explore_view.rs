//! Sectioned landing page mirroring Immich web's Explore tab.
//!
//! Four rows: People (round avatars), Places (embedded map with cluster
//! bubbles), Recently Added (date tiles), Things (tag tiles). Tile clicks
//! invoke caller-provided closures so dispatch lives in `mod.rs`; the Places
//! map reuses `map_view`'s clustering machinery directly.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gdk4::Texture;
use glib::Bytes;
use gtk::prelude::*;

use crate::api_client::{ExploreSection, MapMarker, Person, ThumbnailSize};
use crate::app_context::AppContext;

use super::LibraryWindowUi;
use super::map_view;

type ExploreClick = Rc<dyn Fn(&str, String, String)>;
type PersonClick = Rc<dyn Fn(String, String)>;
type LibraryAction = Rc<dyn Fn()>;

const INITIAL_TILE_COUNT: usize = 16;
const RECENTS_EXPANDED_COUNT: usize = 30;

/// Contains references to individual grid widgets of the explore tab dashboard display.
pub struct ExploreViewParts {
    pub root: gtk::ScrolledWindow,
    pub populated: Rc<Cell<bool>>,
    people_row: gtk::Box,
    recents_grid: gtk::FlowBox,
    /// Stack swapping the embedded Places map between loading / empty / map
    /// states. Replaces the old Places city-tile `FlowBox`.
    places_stack: gtk::Stack,
    /// The embedded libshumate map; built once and reused for the lifetime of
    /// the explore view (the `SimpleMap` carries its viewport + marker layer +
    /// zoom-notify handler, so rebuilding it would stack duplicates).
    places_map: libshumate::SimpleMap,
    things_grid: gtk::FlowBox,
    people_section: gtk::Box,
    recents_section: gtk::Box,
    places_section: gtk::Box,
    things_section: gtk::Box,
    people_spinner: gtk::Spinner,
    recents_spinner: gtk::Spinner,
    places_spinner: gtk::Spinner,
    things_spinner: gtk::Spinner,
    pub people_filter_button: gtk::MenuButton,
    cached_people: Rc<RefCell<Vec<Person>>>,
    cached_people_click: Rc<RefCell<Option<PersonClick>>>,
    /// Decoded avatar textures keyed by person id. `render_people` rebuilds the
    /// row on every explore revisit / face-filter toggle; without this cache
    /// each rebuild re-downloaded every avatar from the server (the visible
    /// "faces reload for 1-2 s on every Library visit" bug). Populated the first
    /// time an avatar decodes; reused instantly on every subsequent tile build.
    person_thumbs: Rc<RefCell<HashMap<String, Texture>>>,
    /// Cached geotagged markers powering the embedded Places map. Replaces the
    /// old `Vec<PlaceItem>` city list; the map re-populates from this on
    /// revisit without re-fetching `/api/map/markers`.
    cached_places: Rc<RefCell<Vec<MapMarker>>>,
    /// Whether [`map_view::populate_map`] has already installed a marker layer
    /// + zoom-notify handler on `places_map`. The `SimpleMap` lives for the
    /// explore view's lifetime, so a second call would stack duplicate layers
    /// and handlers; this guard makes populate-once idempotent. Reset only if
    /// the `SimpleMap` were ever rebuilt (it isn't).
    places_map_populated: Rc<Cell<bool>>,
    /// The "open map" button in the Places section header (hidden until wired).
    places_map_button: gtk::Button,
    /// Callback that opens the full-screen Places map, registered by the
    /// orchestrator via [`wire_places_map`] (so `ui` is in scope for the push).
    on_places_map: Rc<RefCell<Option<LibraryAction>>>,
    pub search_query: Rc<RefCell<String>>,
    cached_ctx: Rc<RefCell<Option<Arc<AppContext>>>>,
    on_favorites: Rc<RefCell<Option<LibraryAction>>>,
    on_archived: Rc<RefCell<Option<LibraryAction>>>,
    on_trash: Rc<RefCell<Option<LibraryAction>>>,
}

/// How to compose a browse-sections view. The Library (Explore) tab and the
/// Search tab both render the same People / Places / Things machinery; they
/// differ only in the chrome that brackets those sections. Rather than fork the
/// section builders (and drift), both tabs call [`build_browse_view`] with the
/// options below and get their own independent [`ExploreViewParts`] (own
/// widgets, own caches, own `populated` guard). Populate/render/drill-in logic
/// is 100% shared.
#[derive(Default)]
pub struct BrowseOptions {
    /// Prepend the iOS-style Favorites / Archived / Trash quick-collection card
    /// at the very top (Library tab only). The Search tab omits it.
    pub include_library_actions: bool,
    /// Include the "Recently Added" tile section (Library tab only). The Search
    /// landing does not surface recents — the Photos tab is already
    /// recency-ordered.
    pub include_recents: bool,
    /// Optional widget inserted above every section (e.g. the Search tab's
    /// search bar). Stays at the top of the scroll column.
    pub lead: Option<gtk::Widget>,
    /// Optional widget appended below every section (e.g. the Search tab's
    /// media-type quick-chip row).
    pub trail: Option<gtk::Widget>,
    /// Include the "Places" city-tile section. The Search tab omits it to keep
    /// the landing lean; the Library tab includes it.
    pub include_places: bool,
    /// Include the "Things" tag-tile section. The Search tab omits it for the
    /// same reason; the Library tab includes it.
    pub include_things: bool,
}

/// Construct the hierarchical panels and containers for the explore dashboard
/// view (Library tab): quick-collection card, People, Places, Recently Added,
/// Things. Thin wrapper over [`build_browse_view`].
pub fn build_explore_view() -> ExploreViewParts {
    build_browse_view(BrowseOptions {
        include_library_actions: true,
        include_recents: true,
        lead: None,
        trail: None,
        include_places: true,
        include_things: true,
    })
}

/// Construct a browse-sections view (People / Places / Things) with optional
/// chrome per [`BrowseOptions`]. Both the Library tab (via [`build_explore_view`])
/// and the Search tab call this, each receiving its own independent
/// [`ExploreViewParts`] that shares all the populate/render code below.
pub fn build_browse_view(opts: BrowseOptions) -> ExploreViewParts {
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();

    // Callback slots for the quick-collection action cards. The orchestrator
    // registers these via `wire_library_actions` after the view is built.
    let on_favorites: Rc<RefCell<Option<LibraryAction>>> = Rc::new(RefCell::new(None));
    let on_archived: Rc<RefCell<Option<LibraryAction>>> = Rc::new(RefCell::new(None));
    let on_trash: Rc<RefCell<Option<LibraryAction>>> = Rc::new(RefCell::new(None));

    let (people_section, people_row, people_spinner, people_filter_button) = build_people_section();
    let (recents_section, recents_grid, recents_spinner) = build_tile_section("Recently Added");
    let (places_section, places_stack, places_map, places_spinner, places_map_button) =
        build_places_section();
    let (things_section, things_grid, things_spinner) = build_tile_section("Things");

    // Optional lead widget (search bar) sits above everything.
    if let Some(lead) = &opts.lead {
        outer.append(lead);
    }

    // Immich-iOS-style quick-collection grid at the very top (Library tab only).
    // Renders immediately (no async data). Shared Links is deferred (no API), so
    // the grid carries Favorites, Archived, Trash. (Backup is reached from the
    // header backup icon on the Photos tab, not from here.)
    if opts.include_library_actions {
        let library_actions = build_library_actions(
            on_favorites.clone(),
            on_archived.clone(),
            on_trash.clone(),
        );
        outer.append(&library_actions);
    }

    outer.append(&people_section);
    if opts.include_places {
        outer.append(&places_section);
    }
    if opts.include_recents {
        outer.append(&recents_section);
    }
    if opts.include_things {
        outer.append(&things_section);
    }

    // Optional trail widget (media-type chip row) below the sections.
    if let Some(trail) = &opts.trail {
        outer.append(trail);
    }

    let root = gtk::ScrolledWindow::builder()
        .child(&outer)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();

    ExploreViewParts {
        root,
        populated: Rc::new(Cell::new(false)),
        people_row,
        recents_grid,
        places_stack,
        places_map,
        things_grid,
        people_section,
        recents_section,
        places_section,
        things_section,
        people_spinner,
        recents_spinner,
        places_spinner,
        things_spinner,
        people_filter_button,
        cached_people: Rc::new(RefCell::new(Vec::new())),
        cached_people_click: Rc::new(RefCell::new(None)),
        person_thumbs: Rc::new(RefCell::new(HashMap::new())),
        cached_places: Rc::new(RefCell::new(Vec::new())),
        places_map_populated: Rc::new(Cell::new(false)),
        places_map_button,
        on_places_map: Rc::new(RefCell::new(None)),
        search_query: Rc::new(RefCell::new(String::new())),
        cached_ctx: Rc::new(RefCell::new(None)),
        on_favorites,
        on_archived,
        on_trash,
    }
}

/// Build the top-of-page quick-collection grid (Favorites / Archived / Trash),
/// mirroring Immich iOS's Library landing header. Two cards per row, each a
/// rounded card with an icon left of its label. Shared Links is deferred.
///
/// Each card invokes its matching slot on click, if registered.
fn build_library_actions(
    on_favorites: Rc<RefCell<Option<LibraryAction>>>,
    on_archived: Rc<RefCell<Option<LibraryAction>>>,
    on_trash: Rc<RefCell<Option<LibraryAction>>>,
) -> gtk::FlowBox {
    let grid = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .row_spacing(8)
        .column_spacing(8)
        .min_children_per_line(2)
        .max_children_per_line(2)
        .homogeneous(true)
        .build();

    grid.append(&library_action_card(
        "emblem-favorite-symbolic",
        "Favorites",
        on_favorites,
    ));
    grid.append(&library_action_card(
        "folder-symbolic",
        "Archived",
        on_archived,
    ));
    grid.append(&library_action_card(
        "user-trash-symbolic",
        "Trash",
        on_trash,
    ));

    grid
}

/// Build a single rounded quick-collection card: icon left of a label, styled
/// with Adwaita's built-in `card` class so it reads as a tappable pill.
fn library_action_card(
    icon_name: &str,
    label_text: &str,
    slot: Rc<RefCell<Option<LibraryAction>>>,
) -> gtk::Button {
    let icon = gtk::Image::builder()
        .icon_name(icon_name)
        .pixel_size(20)
        .build();
    let label = gtk::Label::builder()
        .label(label_text)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(14)
        .margin_bottom(14)
        .margin_start(16)
        .margin_end(16)
        .build();
    content.append(&icon);
    content.append(&label);

    let button = gtk::Button::builder()
        .child(&content)
        .css_classes(["card"])
        .hexpand(true)
        .build();

    button.connect_clicked(move |_| {
        if let Some(cb) = slot.borrow().clone() {
            cb();
        }
    });
    button
}

/// Register the drill-in callbacks for the quick-collection action cards.
///
/// Stores each closure in its shared slot; the corresponding card invokes it on
/// click. Calling again replaces the previous handler. Safe to call after the
/// view is built and mounted.
pub fn wire_library_actions(
    parts: &ExploreViewParts,
    on_favorites: impl Fn() + 'static,
    on_archived: impl Fn() + 'static,
    on_trash: impl Fn() + 'static,
) {
    *parts.on_favorites.borrow_mut() = Some(Rc::new(on_favorites));
    *parts.on_archived.borrow_mut() = Some(Rc::new(on_archived));
    *parts.on_trash.borrow_mut() = Some(Rc::new(on_trash));
}

/// Reveal each section with its spinner active, so the user gets immediate
/// visual feedback that data is on the way. Each `populate_*` call clears
/// its own spinner when results arrive.
pub fn show_loading(parts: &ExploreViewParts) {
    for (section, spinner) in [
        (&parts.people_section, &parts.people_spinner),
        (&parts.recents_section, &parts.recents_spinner),
        (&parts.places_section, &parts.places_spinner),
        (&parts.things_section, &parts.things_spinner),
    ] {
        section.set_visible(true);
        spinner.set_visible(true);
        spinner.start();
    }
}

fn stop_spinner(spinner: &gtk::Spinner) {
    spinner.stop();
    spinner.set_visible(false);
}

/// Build a horizontal scrolled gallery row dedicated to recognized people circles.
fn build_people_section() -> (gtk::Box, gtk::Box, gtk::Spinner, gtk::MenuButton) {
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
    let spinner = gtk::Spinner::builder()
        .visible(false)
        .valign(gtk::Align::Center)
        .build();
    header.append(&spinner);
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
    (section, row, spinner, filter_button)
}

/// Build a flow grid section mapping image tiles for Places or Things category.
fn build_tile_section(title: &str) -> (gtk::Box, gtk::FlowBox, gtk::Spinner) {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .visible(false)
        .build();
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let title_label = heading(title);
    title_label.set_hexpand(true);
    header.append(&title_label);
    let spinner = gtk::Spinner::builder()
        .visible(false)
        .valign(gtk::Align::Center)
        .build();
    header.append(&spinner);
    section.append(&header);
    let grid = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .row_spacing(8)
        .column_spacing(8)
        .min_children_per_line(2)
        .max_children_per_line(20)
        .homogeneous(true)
        .halign(gtk::Align::Start)
        .build();
    section.append(&grid);
    (section, grid, spinner)
}

/// Build the Places section like [`build_tile_section`], but with a small "open
/// map" button in the header (between the title and the spinner) that pushes the
/// full-screen browsable Places map, and an **embedded libshumate map** in place
/// of the old city-tile grid. The map is interactive (pan/zoom in place);
/// tapping a cluster bubble zooms in and tapping a single-asset pin opens the
/// lightbox (both reuse `map_view`'s clustering machinery). A fixed-height
/// stack wraps it so it can't inflate the surrounding page scroll.
///
/// Returns the section box, the loading/empty/map stack, the embedded
/// `SimpleMap`, the header spinner, and the "open map" header button.
fn build_places_section() -> (
    gtk::Box,
    gtk::Stack,
    libshumate::SimpleMap,
    gtk::Spinner,
    gtk::Button,
) {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .visible(false)
        .build();
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let title_label = heading("Places");
    title_label.set_hexpand(true);
    header.append(&title_label);
    let map_button = gtk::Button::builder()
        .icon_name("mark-location-symbolic")
        .tooltip_text("Open map")
        .css_classes(["flat", "circular"])
        .valign(gtk::Align::Center)
        .build();
    header.append(&map_button);
    let spinner = gtk::Spinner::builder()
        .visible(false)
        .valign(gtk::Align::Center)
        .build();
    header.append(&spinner);
    section.append(&header);

    // The embedded map. `build_simple_map` returns a `vexpand`+`hexpand` map;
    // we drop `vexpand` so the fixed-height host below bounds it, and the
    // surrounding Library page keeps scrolling normally (libshumate's own pan
    // gesture would otherwise fight the page's scrollable).
    let simple_map = map_view::build_simple_map();
    simple_map.set_vexpand(false);
    simple_map.set_hexpand(true);

    // Fixed-height host: gives the map a bounded footprint inside the scrolling
    // page. `Overflow::Hidden` + the `card` class give it a tidy rounded frame.
    let map_host = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .height_request(260)
        .hexpand(true)
        .vexpand(false)
        .overflow(gtk::Overflow::Hidden)
        .css_classes(["card"])
        .build();
    map_host.append(&simple_map);

    // Stack swaps between a blank loading slot (the header spinner signals
    // "fetching"), the empty state, and the live map. Height-request on the
    // stack keeps the footprint stable across state transitions so the page
    // doesn't jump when markers arrive.
    let stack = gtk::Stack::builder()
        .hexpand(true)
        .vexpand(false)
        .height_request(260)
        .build();
    let loading = gtk::Box::new(gtk::Orientation::Vertical, 0);
    stack.add_named(&loading, Some("loading"));
    stack.add_named(&map_view::empty_state(), Some("empty"));
    stack.add_named(&map_host, Some("map"));
    stack.set_visible_child_name("loading");
    section.append(&stack);

    (section, stack, simple_map, spinner, map_button)
}

/// Register the callback that opens the full-screen Places map, invoked when the
/// Places header map button is tapped. The orchestrator calls this once after
/// the view is built (guarded by the explore `populated` flag) so `ui` is
/// captured for the nav push. The handler reads through the shared slot, so a
/// later re-registration replaces the target without re-wiring the button.
pub fn wire_places_map(parts: &ExploreViewParts, on_open: impl Fn() + 'static) {
    let first = parts.on_places_map.borrow().is_none();
    *parts.on_places_map.borrow_mut() = Some(Rc::new(on_open));
    if first {
        let slot = parts.on_places_map.clone();
        parts.places_map_button.connect_clicked(move |_| {
            if let Some(cb) = slot.borrow().clone() {
                cb();
            }
        });
    }
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
    stop_spinner(&parts.people_spinner);
    *parts.cached_people.borrow_mut() = people;
    *parts.cached_people_click.borrow_mut() = Some(Rc::new(on_click));
    *parts.cached_ctx.borrow_mut() = Some(ctx.clone());
    render_people(parts, ctx);
}

/// Apply (or clear) the search filter on the people row. Pass an empty string
/// to disable filtering. Caller drives this from the header-bar search entry
/// when the Explore view is the active content stack child.
pub fn set_people_search(parts: &ExploreViewParts, query: &str) {
    // No-op when the query is unchanged. `sync_tab_controls` clears the filter
    // ("") on every tab switch; without this guard that rebuilt the entire
    // people row (tearing down and re-appending every tile) on each switch even
    // when nothing changed. Avatar textures are cached now so a rebuild no
    // longer re-downloads, but skipping the churn entirely is still correct.
    if *parts.search_query.borrow() == query {
        return;
    }
    *parts.search_query.borrow_mut() = query.to_string();
    let Some(ctx) = parts.cached_ctx.borrow().clone() else {
        return;
    };
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
    let query = parts.search_query.borrow().to_ascii_lowercase();
    let filtered: Vec<&Person> = cached
        .iter()
        .filter(|p| show_hidden || !p.is_hidden)
        .filter(|p| show_unnamed || !p.name.is_empty())
        .filter(|p| query.is_empty() || p.name.to_ascii_lowercase().contains(&query))
        .collect();
    parts.people_section.set_visible(!filtered.is_empty());
    let on_click = parts.cached_people_click.borrow().clone();
    let Some(on_click) = on_click else {
        return;
    };
    for person in filtered.into_iter().take(40) {
        let tile = person_tile(
            ctx.clone(),
            person,
            on_click.clone(),
            parts.person_thumbs.clone(),
        );
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
        let active = btn.is_active();
        let hidden_count = parts_b
            .cached_people
            .borrow()
            .iter()
            .filter(|p| p.is_hidden)
            .count();
        log::debug!(
            "show_hidden_faces toggled to {} ({} hidden people in cache)",
            active,
            hidden_count
        );
        {
            let mut cfg = ctx_b.config.write();
            cfg.data.show_hidden_faces = active;
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
        recents_grid: parts.recents_grid.clone(),
        places_stack: parts.places_stack.clone(),
        places_map: parts.places_map.clone(),
        things_grid: parts.things_grid.clone(),
        people_section: parts.people_section.clone(),
        recents_section: parts.recents_section.clone(),
        places_section: parts.places_section.clone(),
        things_section: parts.things_section.clone(),
        people_spinner: parts.people_spinner.clone(),
        recents_spinner: parts.recents_spinner.clone(),
        places_spinner: parts.places_spinner.clone(),
        things_spinner: parts.things_spinner.clone(),
        people_filter_button: parts.people_filter_button.clone(),
        cached_people: parts.cached_people.clone(),
        cached_people_click: parts.cached_people_click.clone(),
        person_thumbs: parts.person_thumbs.clone(),
        cached_places: parts.cached_places.clone(),
        places_map_populated: parts.places_map_populated.clone(),
        places_map_button: parts.places_map_button.clone(),
        on_places_map: parts.on_places_map.clone(),
        search_query: parts.search_query.clone(),
        cached_ctx: parts.cached_ctx.clone(),
        on_favorites: parts.on_favorites.clone(),
        on_archived: parts.on_archived.clone(),
        on_trash: parts.on_trash.clone(),
    }
}

/// Populate the embedded Places map with fetched geotagged markers.
///
/// Caches the markers so subsequent visits don't re-fetch `/api/map/markers`.
/// The embedded `SimpleMap` is populated exactly once per explore view
/// lifetime (see [`ExploreViewParts::places_map_populated`]): installing the
/// marker layer + zoom-notify handler a second time would stack duplicates.
/// The map widget persists across tab revisits, so [`render_cached_places`] is
/// a no-op once populated.
///
/// Replaces the old `populate_places(Vec<PlaceItem>, city-click)` city-tile
/// grid; tapping bubbles now zooms and tapping a single pin opens the lightbox
/// (the city drill lives on the full-screen map opened from the header button).
pub(super) fn populate_places(ui: &Rc<LibraryWindowUi>, parts: &ExploreViewParts, markers: Vec<MapMarker>) {
    stop_spinner(&parts.places_spinner);
    *parts.cached_places.borrow_mut() = markers.clone();
    parts.places_section.set_visible(true);
    if markers.is_empty() {
        parts.places_stack.set_visible_child_name("empty");
        return;
    }
    if !parts.places_map_populated.get() {
        map_view::populate_map(ui, &parts.places_map, markers);
        parts.places_map_populated.set(true);
    }
    parts.places_stack.set_visible_child_name("map");
}

/// Check if places markers are already cached (so the landing load can skip
/// the fetch and re-render the embedded map from cache).
pub fn has_cached_places(parts: &ExploreViewParts) -> bool {
    !parts.cached_places.borrow().is_empty()
}

/// Re-render the embedded Places map from cache, used when navigating back to
/// the Library landing (or after a `refresh_library_surfaces` reset). No
/// re-fetch: if the map was already populated it just stays live; otherwise it
/// is populated now from the cached markers.
pub(super) fn render_cached_places(ui: &Rc<LibraryWindowUi>, parts: &ExploreViewParts) {
    stop_spinner(&parts.places_spinner);
    let markers = parts.cached_places.borrow().clone();
    if markers.is_empty() {
        return;
    }
    parts.places_section.set_visible(true);
    if !parts.places_map_populated.get() {
        map_view::populate_map(ui, &parts.places_map, markers);
        parts.places_map_populated.set(true);
    }
    parts.places_stack.set_visible_child_name("map");
}

/// Populate explore sections: Things tiles + Recently Added tiles.
///
/// Sections by `field_name`:
///   - `exifInfo.city`          -- skipped (populated by `populate_places`)
///   - `createdAt`              -- rendered as "Recently Added" tiles
///   - `smartInfo.objects/tags` -- rendered as "Things" tiles
pub fn populate_explore<F>(
    parts: &ExploreViewParts,
    ctx: Arc<AppContext>,
    sections: Vec<ExploreSection>,
    on_click: F,
) where
    F: Fn(&str, String, String) + 'static,
{
    stop_spinner(&parts.things_spinner);
    stop_spinner(&parts.recents_spinner);
    while let Some(child) = parts.things_grid.first_child() {
        parts.things_grid.remove(&child);
    }
    while let Some(child) = parts.recents_grid.first_child() {
        parts.recents_grid.remove(&child);
    }
    let mut had_things = false;
    let mut had_recents = false;

    log::debug!(
        "Explore sections: [{}]",
        sections
            .iter()
            .map(|s| format!("{}({})", s.field_name, s.items.len()))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let on_click: ExploreClick = Rc::new(on_click);
    for section in sections {
        if section.field_name.contains("city") {
            continue;
        }

        // Immich v3 recently-added section.
        if section.field_name == "createdAt" || section.field_name == "updatedAt" {
            had_recents = true;
            let mut items: Vec<_> = section.items.into_iter().collect();
            items.sort_by(|a, b| b.value.cmp(&a.value));
            render_recents_tiles(parts, &ctx, &items, &on_click, false);
            if items.len() > INITIAL_TILE_COUNT {
                let remaining = items.len().min(RECENTS_EXPANDED_COUNT) - INITIAL_TILE_COUNT;
                let parts_clone = clone_parts_handles(parts);
                let ctx_clone = ctx.clone();
                let on_click_clone = on_click.clone();
                append_see_more_button(&parts.recents_grid, remaining, move || {
                    render_recents_tiles(&parts_clone, &ctx_clone, &items, &on_click_clone, true);
                });
            }
            continue;
        }

        // Only render smartInfo sections as Things tiles.
        if !section.field_name.starts_with("smartInfo") {
            log::debug!("Skipping unknown section '{}'", section.field_name);
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
    parts.recents_section.set_visible(had_recents);
}

/// Render recently-added tiles into the recents grid.
fn render_recents_tiles(
    parts: &ExploreViewParts,
    ctx: &Arc<AppContext>,
    items: &[crate::api_client::ExploreItem],
    on_click: &ExploreClick,
    expanded: bool,
) {
    while let Some(child) = parts.recents_grid.first_child() {
        parts.recents_grid.remove(&child);
    }
    let limit = if expanded {
        RECENTS_EXPANDED_COUNT
    } else {
        INITIAL_TILE_COUNT
    };
    for item in items.iter().take(limit) {
        let label = format_relative_date(&item.value);
        let tile = explore_tile(
            ctx.clone(),
            "recent",
            &label,
            &item.data.id,
            on_click.clone(),
        );
        parts.recents_grid.append(&tile);
    }
}

fn append_action_button<F: Fn() + 'static>(
    grid: &gtk::FlowBox,
    icon_name: &str,
    label_text: &str,
    on_click: F,
) {
    let icon = gtk::Image::builder()
        .icon_name(icon_name)
        .pixel_size(24)
        .halign(gtk::Align::Center)
        .build();
    let label = gtk::Label::builder()
        .label(label_text)
        .css_classes(["caption-heading"])
        .halign(gtk::Align::Center)
        .build();
    // Spacer forces the same min-height as explore tiles.
    let spacer = gtk::Box::builder()
        .css_classes(["mimick-explore-spacer"])
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    content.append(&icon);
    content.append(&label);
    // Overlay the centered label on top of the spacer so the button
    // has the same footprint as a regular tile.
    let overlay = gtk::Overlay::builder()
        .overflow(gtk::Overflow::Hidden)
        .css_classes(["mimick-see-more-tile"])
        .build();
    overlay.set_child(Some(&spacer));
    overlay.add_overlay(&content);

    let btn = gtk::Button::builder()
        .child(&overlay)
        .css_classes(["flat"])
        .build();
    let grid_ref = grid.clone();
    btn.connect_clicked(move |button| {
        if let Some(parent) = button.parent() {
            grid_ref.remove(&parent);
        }
        on_click();
    });
    grid.append(&btn);
}

/// Append a card-sized "See More" tile to a FlowBox grid.
///
/// Matches the dimensions of adjacent explore tiles so the button fills a
/// full card slot rather than appearing as a small inline text link.
fn append_see_more_button<F: Fn() + 'static>(grid: &gtk::FlowBox, remaining: usize, on_expand: F) {
    append_action_button(
        grid,
        "view-more-symbolic",
        &format!("See {remaining} more"),
        on_expand,
    );
}

/// Append a card-sized "Show Less" tile to collapse an expanded section.
fn append_show_less_button<F: Fn() + 'static>(grid: &gtk::FlowBox, on_collapse: F) {
    append_action_button(grid, "go-up-symbolic", "Show Less", on_collapse);
}
fn parse_iso_date(iso: &str) -> Option<(i64, u32, u32, u32, u32, u32)> {
    let parsed = iso
        .replace('T', " ")
        .replace('Z', "")
        .chars()
        .take(19)
        .collect::<String>();

    let parts: Vec<&str> = parsed.split(&['-', ' ', ':'][..]).collect();
    if parts.len() < 6 {
        return None;
    }
    match (
        parts[0].parse::<i64>(),
        parts[1].parse::<u32>(),
        parts[2].parse::<u32>(),
        parts[3].parse::<u32>(),
        parts[4].parse::<u32>(),
        parts[5].parse::<u32>(),
    ) {
        (Ok(y), Ok(mo), Ok(d), Ok(h), Ok(mi), Ok(s)) => Some((y, mo, d, h, mi, s)),
        _ => None,
    }
}

fn compute_epoch_seconds(year: i64, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> i64 {
    let days_in_month = [0u32, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut total_days: i64 = 0;
    for y in 1970..year {
        total_days += if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
    }
    let is_leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    for m in 1..month {
        total_days += days_in_month[m as usize] as i64;
        if m == 2 && is_leap {
            total_days += 1;
        }
    }
    total_days += (day - 1) as i64;
    total_days * 86400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64
}

/// Format an ISO 8601 timestamp into a human-readable relative label.
fn format_relative_date(iso: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let Some((year, month, day, hour, min, sec)) = parse_iso_date(iso) else {
        return iso.chars().take(10).collect();
    };

    let ts = compute_epoch_seconds(year, month, day, hour, min, sec);
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let diff = now_secs - ts;

    if diff < 60 {
        "Just now".to_string()
    } else if diff < 3600 {
        let m = diff / 60;
        format!("{m} min ago")
    } else if diff < 86400 {
        let h = diff / 3600;
        format!("{h}h ago")
    } else if diff < 86400 * 7 {
        let d = diff / 86400;
        format!("{d}d ago")
    } else {
        format!("{year:04}-{month:02}-{day:02}")
    }
}

/// Construct an individual circular avatar widget representing a recognized person face.
fn person_tile(
    ctx: Arc<AppContext>,
    person: &Person,
    on_click: Rc<dyn Fn(String, String)>,
    thumb_cache: Rc<RefCell<HashMap<String, Texture>>>,
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

    spawn_person_thumbnail(ctx, person.id.clone(), avatar, thumb_cache);
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
    // Fixed-height thumbnail container: the Overlay sizes itself from the
    // spacer child (100px) so portrait thumbnails cannot inflate the row
    // height.  The Picture overlay fills that space with ContentFit::Cover.
    let thumb = gtk::Overlay::builder()
        .overflow(gtk::Overflow::Hidden)
        .css_classes(vec!["mimick-explore-tile".to_string()])
        .build();
    let spacer = gtk::Box::builder()
        .css_classes(vec!["mimick-explore-spacer".to_string()])
        .build();
    let picture = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Cover)
        .build();
    thumb.set_child(Some(&spacer));
    thumb.add_overlay(&picture);

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
        .build();
    inner.append(&thumb);
    inner.append(&label);

    let button = gtk::Button::builder()
        .child(&inner)
        .css_classes(vec!["flat".to_string()])
        .build();

    let value_owned = value.to_string();
    let asset_id_owned = asset_id.to_string();
    button.connect_clicked(move |_| on_click(kind, value_owned.clone(), asset_id_owned.clone()));

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
///
/// Decoded avatar textures are cached in `thumb_cache` (person id → texture) for
/// the lifetime of the explore view. A cache hit paints synchronously and skips
/// the network entirely, so rebuilding the people row (every Library revisit or
/// face-filter toggle) is instant instead of re-downloading every avatar.
fn spawn_person_thumbnail(
    ctx: Arc<AppContext>,
    person_id: String,
    picture: gtk::Picture,
    thumb_cache: Rc<RefCell<HashMap<String, Texture>>>,
) {
    if let Some(texture) = thumb_cache.borrow().get(&person_id) {
        picture.set_paintable(Some(texture));
        return;
    }
    glib::timeout_add_local_once(std::time::Duration::from_millis(120), move || {
        glib::MainContext::default().spawn_local(async move {
            // Re-check the cache: another tile for the same person (or a rebuild
            // that raced this timeout) may have populated it while we waited.
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
                thumb_cache
                    .borrow_mut()
                    .insert(person_id, texture.clone());
                picture.set_paintable(Some(&texture));
            }
        });
    });
}
