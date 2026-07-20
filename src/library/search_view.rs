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

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use crate::library::explore_view::{self, BrowseOptions, ExploreViewParts};

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
}

/// Build the Search tab view. Returns the root widget, the embedded browse
/// sections, and unset handler slots.
pub fn build_search_view() -> SearchViewParts {
    let on_search: SearchSlot = Rc::new(RefCell::new(None));
    let on_media_chip: MediaChipSlot = Rc::new(RefCell::new(None));
    let on_filter: FilterSlot = Rc::new(RefCell::new(None));

    // The search bar (lead) and media-type chip row (trail) bracket the shared
    // browse sections. The Search tab omits the Library-only quick-collection
    // card and the "Recently Added" section.
    let search_bar = build_search_field(&on_search, &on_filter);
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
    }
}

/// Top search field: a rounded search entry with a trailing **Filters** button.
/// Pressing Enter fires the search slot with the trimmed query; the Filters
/// button fires the filter slot (opens the advanced-filters dialog).
fn build_search_field(on_search: &SearchSlot, on_filter: &FilterSlot) -> gtk::Widget {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();

    let entry = gtk::SearchEntry::builder()
        // Natural-language example placeholder signals this is smart search.
        .placeholder_text("Search — \"sunset on the beach\"")
        .hexpand(true)
        .build();

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

    // A single, honest Filters button. Phase 2 replaces the dialog it opens
    // with a grouped bottom sheet; for now one entry point → the existing
    // advanced-filters dialog.
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

    row.append(&entry);
    row.append(&filter_button);
    row.upcast::<gtk::Widget>()
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
}
