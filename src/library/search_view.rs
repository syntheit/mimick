//! Search tab view — a Google-Photos-style browse surface.
//!
//! Top → bottom:
//!
//! 1. A search bar (rounded `GtkSearchEntry` + a single **Filters** button that
//!    opens the advanced-filters dialog). Submitting (Enter) fires the search
//!    callback with the trimmed query → smart search.
//! 2. A **People** avatar row, a **Places** grid, and a **Categories / Things**
//!    grid — the exact browse sections the Library tab renders, hosted here via
//!    the shared [`explore_view::build_browse_view`] builder (its own
//!    independent [`ExploreViewParts`], so the Library tab is unaffected).
//! 3. A slim **media-type quick-chip** row (Videos / Favorites / Not in an album
//!    / Screenshots) for one-tap drill-ins.
//!
//! The browse sections are populated by the orchestrator (`mod.rs`) via the
//! embedded [`SearchViewParts::browse`] using the same `populate_*` functions
//! the Library tab uses. Handler *slots* for the search bar, Filters button, and
//! media chips are filled after construction with the `set_on_*` helpers.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;

use crate::library::explore_view::{self, BrowseOptions, ExploreViewParts};

/// Which text-search backend a submitted query is routed to. Selected via the
/// leading dropdown in the search bar; read by the orchestrator's `on_search`
/// wiring (`connect_search`) to pick the matching [`LibrarySource`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SearchMode {
    /// CLIP smart search (default) → `LibrarySource::SmartSearch`.
    #[default]
    Smart,
    /// OCR text-in-photos search → `LibrarySource::OcrSearch`.
    Text,
    /// Filename substring search → `LibrarySource::MetadataSearch`.
    Filename,
}

impl SearchMode {
    /// The dropdown option order (index maps 1:1 to this slice).
    const ORDER: [SearchMode; 3] = [SearchMode::Smart, SearchMode::Text, SearchMode::Filename];

    /// Label shown in the leading mode dropdown. "Text in photos" spells out
    /// that this is OCR so users understand why it's a separate mode.
    fn label(self) -> &'static str {
        match self {
            SearchMode::Smart => "Smart",
            SearchMode::Text => "Text in photos",
            SearchMode::Filename => "Filename",
        }
    }

    /// Placeholder tailored to the active mode, teaching what each one matches.
    fn placeholder(self) -> &'static str {
        match self {
            SearchMode::Smart => "Search — \"sunset on the beach\"",
            SearchMode::Text => "Text in photos — \"gate 22\", a receipt total…",
            SearchMode::Filename => "Filename — \"IMG_1234\"",
        }
    }

    fn from_index(i: u32) -> SearchMode {
        Self::ORDER.get(i as usize).copied().unwrap_or_default()
    }
}

/// One-tap media-type quick chips shown below the browse sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaChip {
    Videos,
    Favorites,
    NotInAlbum,
    Screenshots,
}

impl MediaChip {
    /// Human-facing chip label.
    fn label(self) -> &'static str {
        match self {
            MediaChip::Videos => "Videos",
            MediaChip::Favorites => "Favorites",
            MediaChip::NotInAlbum => "Not in an album",
            MediaChip::Screenshots => "Screenshots",
        }
    }

    /// Symbolic icon name for the chip. Standard Adwaita symbolic icons.
    fn icon(self) -> &'static str {
        match self {
            MediaChip::Videos => "video-x-generic-symbolic",
            MediaChip::Favorites => "emblem-favorite-symbolic",
            MediaChip::NotInAlbum => "list-remove-symbolic",
            MediaChip::Screenshots => "camera-photo-symbolic",
        }
    }

    /// Chips in display order (left → right).
    fn all() -> [MediaChip; 4] {
        [
            MediaChip::Videos,
            MediaChip::Favorites,
            MediaChip::NotInAlbum,
            MediaChip::Screenshots,
        ]
    }
}

/// Boxed callback slots, filled by the orchestrator after `build_search_view`.
type SearchSlot = Rc<RefCell<Option<Box<dyn Fn(String)>>>>;
type MediaChipSlot = Rc<RefCell<Option<Box<dyn Fn(MediaChip)>>>>;
type FilterSlot = Rc<RefCell<Option<Box<dyn Fn()>>>>;
/// Handler slot for tapping a recent-search row (re-run that query).
type RecentSlot = Rc<RefCell<Option<Box<dyn Fn(String)>>>>;
/// Handler slot for the inline ✕ on a recent row (forget that query).
type RecentRemoveSlot = Rc<RefCell<Option<Box<dyn Fn(String)>>>>;
/// Handler slot for the "Clear" action (forget all recents).
type RecentsClearSlot = Rc<RefCell<Option<Box<dyn Fn()>>>>;

/// Widgets and handler slots for the Search tab.
///
/// `root` is the top-level widget to place in the Search tab. `browse` is the
/// embedded browse-sections view (People / Places / Things); the orchestrator
/// populates it with the same `explore_view::populate_*` functions the Library
/// tab uses. The slots hold optional handlers installed after construction.
pub struct SearchViewParts {
    /// Top-level widget to place in the Search tab.
    pub root: gtk::Widget,
    /// Shared browse sections (People / Places / Things), hosted inside `root`.
    /// Populated by the orchestrator on first Search-tab visit.
    pub browse: ExploreViewParts,
    /// Query text submitted (Enter pressed in the search field).
    pub on_search: SearchSlot,
    /// A media-type quick chip was tapped.
    pub on_media_chip: MediaChipSlot,
    /// The leading Filters button was tapped.
    pub on_filter: FilterSlot,
    /// A recent-search row was tapped (re-run the query with the current mode).
    pub on_recent: RecentSlot,
    /// The ✕ on a recent-search row was tapped (forget just that query).
    pub on_recent_removed: RecentRemoveSlot,
    /// The recents "Clear" action was tapped (forget all recents).
    pub on_recents_cleared: RecentsClearSlot,
    /// The currently-selected text-search mode (Smart / Text / Filename).
    mode: Rc<Cell<SearchMode>>,
    /// The revealer + list handle for the recents section, so the orchestrator
    /// can repopulate it after loads/submits and focus toggles reveal it.
    recents: RecentsUi,
}

impl SearchViewParts {
    /// Install the search-submit handler. Invoked with the trimmed query text
    /// when the user presses Enter in the search field (empty queries are
    /// suppressed at the callsite).
    pub fn set_on_search(&self, f: impl Fn(String) + 'static) {
        *self.on_search.borrow_mut() = Some(Box::new(f));
    }

    /// Install the media-chip handler, invoked with the tapped [`MediaChip`].
    pub fn set_on_media_chip(&self, f: impl Fn(MediaChip) + 'static) {
        *self.on_media_chip.borrow_mut() = Some(Box::new(f));
    }

    /// Install the Filters-button handler (opens the advanced-filters dialog).
    pub fn set_on_filter(&self, f: impl Fn() + 'static) {
        *self.on_filter.borrow_mut() = Some(Box::new(f));
    }

    /// Install the recent-search tap handler (re-run the tapped query).
    pub fn set_on_recent(&self, f: impl Fn(String) + 'static) {
        *self.on_recent.borrow_mut() = Some(Box::new(f));
    }

    /// Install the recent-search remove (✕) handler.
    pub fn set_on_recent_removed(&self, f: impl Fn(String) + 'static) {
        *self.on_recent_removed.borrow_mut() = Some(Box::new(f));
    }

    /// Install the recents "Clear" handler.
    pub fn set_on_recents_cleared(&self, f: impl Fn() + 'static) {
        *self.on_recents_cleared.borrow_mut() = Some(Box::new(f));
    }

    /// The text-search mode currently selected in the leading dropdown.
    pub fn search_mode(&self) -> SearchMode {
        self.mode.get()
    }

    /// Rebuild the recents list from `queries` (most-recent first). Called by the
    /// orchestrator on Search-tab load and after each submit/remove/clear. The
    /// section reveals only while the search field is empty and focused; an empty
    /// `queries` hides it outright.
    pub fn set_recents(&self, queries: &[String]) {
        rebuild_recents_list(&self.recents, queries, &self.on_recent_removed);
    }
}

/// The recents section widgets the orchestrator drives: a revealer wrapping the
/// list, the list itself, and shared state tracking focus/emptiness so the
/// reveal logic lives in one place.
#[derive(Clone)]
struct RecentsUi {
    /// Wraps the recents list; only shown when focused + empty + non-empty list.
    revealer: gtk::Revealer,
    /// The `boxed-list` holding one row per recent query plus a Clear row.
    list: gtk::ListBox,
    /// Whether the search field currently holds focus.
    focused: Rc<Cell<bool>>,
    /// Whether the search field is currently empty.
    empty: Rc<Cell<bool>>,
    /// Whether there is at least one recent query to show.
    has_items: Rc<Cell<bool>>,
    /// Row → query map for the current rows, rebuilt on each `set_recents`. The
    /// single `row-activated` handler reads this to dispatch (avoids restacking
    /// a fresh handler per rebuild, which would leak). The `None` variant marks
    /// the trailing "Clear" row.
    rows: Rc<RefCell<Vec<(gtk::ListBoxRow, Option<String>)>>>,
}

impl RecentsUi {
    /// Reveal the recents list only when the field is focused, empty, and there
    /// is something to show — matching Google Photos (recents appear on focus).
    fn update_visibility(&self) {
        let show = self.focused.get() && self.empty.get() && self.has_items.get();
        self.revealer.set_reveal_child(show);
    }
}

/// Build the Search tab view. Returns the root widget, the embedded browse
/// sections, and unset handler slots.
pub fn build_search_view() -> SearchViewParts {
    let on_search: SearchSlot = Rc::new(RefCell::new(None));
    let on_media_chip: MediaChipSlot = Rc::new(RefCell::new(None));
    let on_filter: FilterSlot = Rc::new(RefCell::new(None));
    let on_recent: RecentSlot = Rc::new(RefCell::new(None));
    let on_recent_removed: RecentRemoveSlot = Rc::new(RefCell::new(None));
    let on_recents_cleared: RecentsClearSlot = Rc::new(RefCell::new(None));
    let mode: Rc<Cell<SearchMode>> = Rc::new(Cell::new(SearchMode::default()));

    // The search bar (lead) and media-type chip row (trail) bracket the shared
    // browse sections. The Search tab omits the Library-only quick-collection
    // card and the "Recently Added" section.
    let (search_bar, recents) = build_search_field(
        &on_search,
        &on_filter,
        &on_recent,
        &on_recents_cleared,
        &mode,
    );
    let media_row = build_media_chip_row(&on_media_chip);

    let browse = explore_view::build_browse_view(BrowseOptions {
        include_library_actions: false,
        include_recents: false,
        lead: Some(search_bar),
        trail: Some(media_row),
        include_places: false,
        include_things: false,
    });

    let root = browse.root.clone().upcast::<gtk::Widget>();

    SearchViewParts {
        root,
        browse,
        on_search,
        on_media_chip,
        on_filter,
        on_recent,
        on_recent_removed,
        on_recents_cleared,
        mode,
        recents,
    }
}

/// Top search area: a leading mode dropdown + rounded search entry + trailing
/// **Filters** button, with a **Recent searches** list revealed below on focus.
/// Pressing Enter fires the search slot with the trimmed query; the Filters
/// button fires the filter slot. Returns the column widget and a [`RecentsUi`]
/// handle the orchestrator uses to populate/refresh the recents list.
fn build_search_field(
    on_search: &SearchSlot,
    on_filter: &FilterSlot,
    on_recent: &RecentSlot,
    on_recents_cleared: &RecentsClearSlot,
    mode: &Rc<Cell<SearchMode>>,
) -> (gtk::Widget, RecentsUi) {
    // Vertical column: [ mode | entry | filters ] row, then the recents revealer.
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();

    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();

    let entry = gtk::SearchEntry::builder()
        // Natural-language example placeholder signals this is smart search.
        .placeholder_text(SearchMode::Smart.placeholder())
        .hexpand(true)
        .build();

    // --- Leading search-type dropdown: Smart / Text in photos / Filename ---
    let mode_dropdown = build_mode_dropdown(mode, &entry);

    // `SearchEntry::activate` fires on Enter. Submit the trimmed text; skip
    // empty queries so the orchestrator never runs a blank search.
    let slot = on_search.clone();
    entry.connect_activate(move |entry| {
        let text = entry.text().trim().to_string();
        if text.is_empty() {
            return;
        }
        if let Some(cb) = slot.borrow().as_ref() {
            cb(text);
        }
    });

    // A single, honest Filters button → the guided, grouped filters sheet.
    let filter_content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    // `view-more-symbolic` is the established filter/config glyph in this
    // codebase (Library header, explore people filter) and is guaranteed
    // present; a bespoke "funnel" name risks a missing-icon blank.
    filter_content.append(&gtk::Image::from_icon_name("view-more-symbolic"));
    filter_content.append(&gtk::Label::new(Some("Filters")));
    let filter_button = gtk::Button::builder()
        .child(&filter_content)
        .tooltip_text("Filter by date, location, camera, and more")
        .css_classes(["pill", "flat"])
        .valign(gtk::Align::Center)
        .build();
    let filter_slot = on_filter.clone();
    filter_button.connect_clicked(move |_| {
        if let Some(cb) = filter_slot.borrow().as_ref() {
            cb();
        }
    });

    row.append(&mode_dropdown);
    row.append(&entry);
    row.append(&filter_button);
    column.append(&row);

    // --- Recent searches, revealed below the bar on focus (when empty) ---
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .child(&list)
        .build();
    column.append(&revealer);

    let recents = RecentsUi {
        revealer,
        list,
        focused: Rc::new(Cell::new(false)),
        empty: Rc::new(Cell::new(true)),
        has_items: Rc::new(Cell::new(false)),
        rows: Rc::new(RefCell::new(Vec::new())),
    };

    // Focus + text drive the reveal (Google Photos: recents on focus, hidden as
    // soon as you type). The focus controller lives on the whole `column` (bar +
    // recents list): `enter`/`leave` fire only on the subtree boundary, so focus
    // moving from the entry onto a recent row does NOT collapse the list mid-tap
    // (a per-entry watch would). It also correctly covers the SearchEntry's
    // internal text widget, which owns the real focus.
    {
        let focus = gtk::EventControllerFocus::new();
        let recents_enter = recents.clone();
        focus.connect_enter(move |_| {
            recents_enter.focused.set(true);
            recents_enter.update_visibility();
        });
        let recents_leave = recents.clone();
        focus.connect_leave(move |_| {
            recents_leave.focused.set(false);
            recents_leave.update_visibility();
        });
        column.add_controller(focus);
    }
    {
        let recents = recents.clone();
        // `search-changed` is debounced by GtkSearchEntry; that's fine here — we
        // only need "is the field empty?" to gate the reveal, not per-keystroke.
        entry.connect_search_changed(move |e| {
            recents.empty.set(e.text().trim().is_empty());
            recents.update_visibility();
        });
    }

    // Connect the list's activation exactly once, dispatching via the shared
    // `rows` map so repeated `set_recents` rebuilds never restack handlers.
    // The row → query lookup and slots are read live at click time.
    {
        let rows = recents.rows.clone();
        let on_recent = on_recent.clone();
        let on_recents_cleared = on_recents_cleared.clone();
        recents.list.connect_row_activated(move |_, activated| {
            let query = rows
                .borrow()
                .iter()
                .find(|(row, _)| row == activated)
                .map(|(_, q)| q.clone());
            match query {
                // A recent-query row: re-run it.
                Some(Some(query)) => {
                    if let Some(cb) = on_recent.borrow().as_ref() {
                        cb(query);
                    }
                }
                // The trailing Clear row.
                Some(None) => {
                    if let Some(cb) = on_recents_cleared.borrow().as_ref() {
                        cb();
                    }
                }
                None => {}
            }
        });
    }

    (column.upcast::<gtk::Widget>(), recents)
}

/// Build the leading search-type dropdown (Smart / Text in photos / Filename).
/// Writes the pick into `mode` and re-hints the entry placeholder to match.
fn build_mode_dropdown(mode: &Rc<Cell<SearchMode>>, entry: &gtk::SearchEntry) -> gtk::DropDown {
    let labels: Vec<&str> = SearchMode::ORDER.iter().map(|m| m.label()).collect();
    let model = gtk::StringList::new(&labels);
    let dropdown = gtk::DropDown::builder()
        .model(&model)
        .tooltip_text("Choose what to search: Smart (AI), Text in photos (OCR), or Filename")
        .valign(gtk::Align::Center)
        .build();

    let mode = mode.clone();
    let entry = entry.clone();
    dropdown.connect_selected_notify(move |d| {
        let selected = SearchMode::from_index(d.selected());
        mode.set(selected);
        entry.set_placeholder_text(Some(selected.placeholder()));
    });
    dropdown
}

/// Rebuild the recents `ListBox` in place: one activatable row per query
/// (history glyph + text, with an inline ✕), then a trailing "Clear" row. Hides
/// the whole section when `queries` is empty. The row → query map is stored in
/// `recents.rows` for the (once-connected) `row-activated` dispatcher; the ✕
/// buttons carry their own click handlers wired here.
fn rebuild_recents_list(
    recents: &RecentsUi,
    queries: &[String],
    on_recent_removed: &RecentRemoveSlot,
) {
    while let Some(child) = recents.list.first_child() {
        recents.list.remove(&child);
    }

    let mut rows = recents.rows.borrow_mut();
    rows.clear();

    for query in queries {
        let row = build_recent_row(query, on_recent_removed);
        rows.push((row.clone(), Some(query.clone())));
        recents.list.append(&row);
    }

    let has_items = !queries.is_empty();
    if has_items {
        // Trailing "Clear" row spanning the list; recorded with a `None` query so
        // the dispatcher knows to fire the clear slot.
        let clear = gtk::ListBoxRow::builder().activatable(true).build();
        let clear_label = gtk::Label::builder()
            .label("Clear")
            .halign(gtk::Align::Center)
            .margin_top(10)
            .margin_bottom(10)
            .css_classes(["dim-label"])
            .build();
        clear.set_child(Some(&clear_label));
        rows.push((clear.clone(), None));
        recents.list.append(&clear);
    }
    drop(rows);

    recents.has_items.set(has_items);
    recents.update_visibility();
}

/// A single recent-search row: a history glyph, the query text, and an inline ✕
/// that forgets just that query. Activating the row (handled by the ListBox)
/// re-runs the query.
fn build_recent_row(query: &str, on_recent_removed: &RecentRemoveSlot) -> gtk::ListBoxRow {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(10)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(8)
        .build();

    content.append(
        &gtk::Image::builder()
            .icon_name("document-open-recent-symbolic")
            .css_classes(["dim-label"])
            .build(),
    );
    content.append(
        &gtk::Label::builder()
            .label(query)
            .halign(gtk::Align::Start)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build(),
    );

    // Inline remove (✕): a flat, circular button that forgets just this query.
    // It handles its own click and does not activate the row.
    let remove = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .css_classes(["flat", "circular"])
        .valign(gtk::Align::Center)
        .tooltip_text("Remove from recent searches")
        .build();
    {
        let q = query.to_string();
        let slot = on_recent_removed.clone();
        remove.connect_clicked(move |_| {
            if let Some(cb) = slot.borrow().as_ref() {
                cb(q.clone());
            }
        });
    }
    content.append(&remove);

    let row = gtk::ListBoxRow::builder().activatable(true).build();
    row.set_child(Some(&content));
    row
}

/// Slim horizontally-scrolling media-type quick-chip row. Each chip is an
/// icon+label pill button that fires the media-chip slot with its [`MediaChip`].
fn build_media_chip_row(on_media_chip: &MediaChipSlot) -> gtk::Widget {
    let chips = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();

    for chip in MediaChip::all() {
        chips.append(&build_media_chip(chip, on_media_chip));
    }

    gtk::ScrolledWindow::builder()
        .child(&chips)
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .build()
        .upcast::<gtk::Widget>()
}

/// A single media-type chip: a flat pill button with a leading symbolic icon
/// and a label.
fn build_media_chip(chip: MediaChip, on_media_chip: &MediaChipSlot) -> gtk::Button {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    content.append(&gtk::Image::from_icon_name(chip.icon()));
    content.append(&gtk::Label::new(Some(chip.label())));

    // "pill" gives the rounded capsule shape in Adwaita; keep it flat so it
    // reads as a chip rather than a raised button.
    let button = gtk::Button::builder()
        .child(&content)
        .css_classes(["pill", "flat"])
        .build();

    let slot = on_media_chip.clone();
    button.connect_clicked(move |_| {
        if let Some(cb) = slot.borrow().as_ref() {
            cb(chip);
        }
    });
    button
}

#[cfg(test)]
mod tests {
    use super::*;

    // Logic-only tests: enum → label/icon mappings and ordering. No GTK widgets
    // are constructed, so these run headless in the shared suite.

    #[test]
    fn media_chip_labels() {
        assert_eq!(MediaChip::Videos.label(), "Videos");
        assert_eq!(MediaChip::Favorites.label(), "Favorites");
        assert_eq!(MediaChip::NotInAlbum.label(), "Not in an album");
        assert_eq!(MediaChip::Screenshots.label(), "Screenshots");
    }

    #[test]
    fn media_chip_icons_are_symbolic() {
        for chip in MediaChip::all() {
            assert!(
                chip.icon().ends_with("-symbolic"),
                "media chip {chip:?} icon {} is not symbolic",
                chip.icon()
            );
        }
    }

    #[test]
    fn media_chip_order_is_stable() {
        assert_eq!(
            MediaChip::all(),
            [
                MediaChip::Videos,
                MediaChip::Favorites,
                MediaChip::NotInAlbum,
                MediaChip::Screenshots,
            ]
        );
    }

    #[test]
    fn search_mode_default_is_smart() {
        assert_eq!(SearchMode::default(), SearchMode::Smart);
    }

    #[test]
    fn search_mode_index_round_trips_and_clamps() {
        for (i, expected) in SearchMode::ORDER.iter().enumerate() {
            assert_eq!(SearchMode::from_index(i as u32), *expected);
        }
        // Out-of-range indices fall back to the default (Smart) rather than panic.
        assert_eq!(SearchMode::from_index(99), SearchMode::Smart);
    }

    #[test]
    fn search_mode_text_label_names_ocr_clearly() {
        // The Text mode must spell out that it's OCR so users understand it.
        assert_eq!(SearchMode::Text.label(), "Text in photos");
    }
}
