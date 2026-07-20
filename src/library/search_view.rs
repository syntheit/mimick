//! Search tab view.
//!
//! Builds the Search tab UI to match Immich's iOS mobile layout (top → bottom):
//!
//! 1. A rounded search field with a filter-config icon; submitting (Enter)
//!    fires the search callback.
//! 2. A horizontally-scrolling row of filter chips (People / Location / Camera
//!    / Date / Media type); tapping a chip fires the chip callback so the
//!    orchestrator can open the matching picker.
//! 3. A centered empty-state (large dim icon + caption) shown when no search is
//!    active.
//! 4. A rounded "quick links" card (a `boxed-list` `ListBox`) with four rows —
//!    Recently taken / Recently added / Videos / Favorites — each firing the
//!    quick-link callback with an identifier.
//!
//! The module is self-contained: it references no private items from sibling
//! modules and exposes handler *slots* the orchestrator fills after
//! construction. Wire-up is done via the setter methods on [`SearchViewParts`]
//! (or by writing directly into the public `Rc<RefCell<..>>` slots).

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

/// Quick-link rows in the search card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuickLink {
    RecentlyTaken,
    RecentlyAdded,
    Videos,
    Favorites,
}

/// Filter chips shown above the empty-state / results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchChip {
    People,
    Location,
    Camera,
    Date,
    MediaType,
}

impl SearchChip {
    /// Human-facing chip label (Immich mobile wording).
    fn label(self) -> &'static str {
        match self {
            SearchChip::People => "People",
            SearchChip::Location => "Location",
            SearchChip::Camera => "Camera",
            SearchChip::Date => "Date",
            SearchChip::MediaType => "Media type",
        }
    }

    /// Symbolic icon name for the chip. All names below ship with the Adwaita
    /// icon theme (verified against the standard symbolic set).
    fn icon(self) -> &'static str {
        match self {
            SearchChip::People => "system-users-symbolic",
            SearchChip::Location => "mark-location-symbolic",
            SearchChip::Camera => "camera-photo-symbolic",
            SearchChip::Date => "x-office-calendar-symbolic",
            SearchChip::MediaType => "video-x-generic-symbolic",
        }
    }

    /// Chips in display order (left → right).
    fn all() -> [SearchChip; 5] {
        [
            SearchChip::People,
            SearchChip::Location,
            SearchChip::Camera,
            SearchChip::Date,
            SearchChip::MediaType,
        ]
    }
}

impl QuickLink {
    /// Human-facing row label.
    fn label(self) -> &'static str {
        match self {
            QuickLink::RecentlyTaken => "Recently taken",
            QuickLink::RecentlyAdded => "Recently added",
            QuickLink::Videos => "Videos",
            QuickLink::Favorites => "Favorites",
        }
    }

    /// Symbolic icon name for the row prefix. Standard Adwaita symbolic icons.
    fn icon(self) -> &'static str {
        match self {
            QuickLink::RecentlyTaken => "document-open-recent-symbolic",
            QuickLink::RecentlyAdded => "list-add-symbolic",
            QuickLink::Videos => "video-x-generic-symbolic",
            QuickLink::Favorites => "emblem-favorite-symbolic",
        }
    }

    /// Quick-links in display order (top → bottom).
    fn all() -> [QuickLink; 4] {
        [
            QuickLink::RecentlyTaken,
            QuickLink::RecentlyAdded,
            QuickLink::Videos,
            QuickLink::Favorites,
        ]
    }
}

/// Boxed callback slots, filled by the orchestrator after `build_search_view`.
type SearchSlot = Rc<RefCell<Option<Box<dyn Fn(String)>>>>;
type QuickLinkSlot = Rc<RefCell<Option<Box<dyn Fn(QuickLink)>>>>;
type ChipSlot = Rc<RefCell<Option<Box<dyn Fn(SearchChip)>>>>;
type FilterSlot = Rc<RefCell<Option<Box<dyn Fn()>>>>;

/// Widgets and handler slots for the Search tab.
///
/// `root` is the top-level widget to place in the Search tab. The three slots
/// hold optional handlers the orchestrator installs after construction; wire
/// them with the `set_on_*` helpers (or assign into the slots directly).
pub struct SearchViewParts {
    /// Top-level widget to place in the Search tab.
    pub root: gtk::Widget,
    /// Query text submitted (Enter pressed in the search field).
    pub on_search: SearchSlot,
    /// A quick-link card row was activated.
    pub on_quick_link: QuickLinkSlot,
    /// A filter chip was tapped.
    pub on_chip: ChipSlot,
    /// The leading filter (hamburger) button was tapped.
    pub on_filter: FilterSlot,
}

impl SearchViewParts {
    /// Install the search-submit handler. Invoked with the trimmed query text
    /// when the user presses Enter in the search field (empty queries are
    /// suppressed at the callsite).
    pub fn set_on_search(&self, f: impl Fn(String) + 'static) {
        *self.on_search.borrow_mut() = Some(Box::new(f));
    }

    /// Install the quick-link handler, invoked with the tapped [`QuickLink`].
    pub fn set_on_quick_link(&self, f: impl Fn(QuickLink) + 'static) {
        *self.on_quick_link.borrow_mut() = Some(Box::new(f));
    }

    /// Install the chip handler, invoked with the tapped [`SearchChip`].
    pub fn set_on_chip(&self, f: impl Fn(SearchChip) + 'static) {
        *self.on_chip.borrow_mut() = Some(Box::new(f));
    }

    pub fn set_on_filter(&self, f: impl Fn() + 'static) {
        *self.on_filter.borrow_mut() = Some(Box::new(f));
    }
}

/// Build the Search tab view. Returns the root widget plus unset handler slots.
pub fn build_search_view() -> SearchViewParts {
    let on_search: SearchSlot = Rc::new(RefCell::new(None));
    let on_quick_link: QuickLinkSlot = Rc::new(RefCell::new(None));
    let on_chip: ChipSlot = Rc::new(RefCell::new(None));
    let on_filter: FilterSlot = Rc::new(RefCell::new(None));

    // Vertical column that scrolls as a whole; the chip row scrolls
    // independently on its own horizontal axis.
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(12)
        .margin_bottom(16)
        .margin_start(12)
        .margin_end(12)
        .build();

    column.append(&build_search_field(&on_search, &on_filter));
    column.append(&build_chip_row(&on_chip));
    column.append(&build_empty_state());
    column.append(&build_quick_links_card(&on_quick_link));

    let root = gtk::ScrolledWindow::builder()
        .child(&column)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .build();

    SearchViewParts {
        root: root.upcast::<gtk::Widget>(),
        on_search,
        on_quick_link,
        on_chip,
        on_filter,
    }
}

/// Top search field: a filter-config button beside a rounded search entry.
/// Pressing Enter fires the search slot with the trimmed query.
fn build_search_field(on_search: &SearchSlot, on_filter: &FilterSlot) -> gtk::Widget {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();

    // Leading filter-config button (matches the iOS layout's leading glyph).
    // Fires the filter slot so the orchestrator can open the filter sheet.
    let filter_button = gtk::Button::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Search filters")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    let filter_slot = on_filter.clone();
    filter_button.connect_clicked(move |_| {
        if let Some(cb) = filter_slot.borrow().as_ref() {
            cb();
        }
    });

    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Search your photos")
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

    row.append(&filter_button);
    row.append(&entry);
    row.upcast::<gtk::Widget>()
}

/// Horizontally-scrolling chip row. Each chip is an icon+label pill button that
/// fires the chip slot with its [`SearchChip`] on click.
fn build_chip_row(on_chip: &ChipSlot) -> gtk::Widget {
    let chips = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();

    for chip in SearchChip::all() {
        chips.append(&build_chip(chip, on_chip));
    }

    gtk::ScrolledWindow::builder()
        .child(&chips)
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .build()
        .upcast::<gtk::Widget>()
}

/// A single filter chip: a flat pill button with a leading symbolic icon and a
/// label, styled as a rounded pill.
fn build_chip(chip: SearchChip, on_chip: &ChipSlot) -> gtk::Button {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    let icon = gtk::Image::from_icon_name(chip.icon());
    let label = gtk::Label::new(Some(chip.label()));
    content.append(&icon);
    content.append(&label);

    // "pill" gives the rounded capsule shape in Adwaita; keep it flat so it
    // reads as a chip rather than a raised button.
    let button = gtk::Button::builder()
        .child(&content)
        .css_classes(["pill", "flat"])
        .build();

    let slot = on_chip.clone();
    button.connect_clicked(move |_| {
        if let Some(cb) = slot.borrow().as_ref() {
            cb(chip);
        }
    });
    button
}

/// Centered empty-state shown when no search is active: a large dim icon with a
/// caption. Uses `AdwStatusPage` for the standard centered treatment.
fn build_empty_state() -> gtk::Widget {
    adw::StatusPage::builder()
        .icon_name("camera-photo-symbolic")
        .title("Search your photos and videos")
        .vexpand(true)
        .build()
        .upcast::<gtk::Widget>()
}

/// Quick-links card: a rounded `boxed-list` `ListBox` with one activatable
/// `AdwActionRow` per [`QuickLink`]. Activating a row fires the quick-link slot.
fn build_quick_links_card(on_quick_link: &QuickLinkSlot) -> gtk::Widget {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    for link in QuickLink::all() {
        let row = adw::ActionRow::builder()
            .title(link.label())
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(link.icon()));

        let slot = on_quick_link.clone();
        row.connect_activated(move |_| {
            if let Some(cb) = slot.borrow().as_ref() {
                cb(link);
            }
        });
        list.append(&row);
    }

    list.upcast::<gtk::Widget>()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Logic-only tests: enum → label/icon mappings and ordering. No GTK widgets
    // are constructed, so these run headless in the shared suite.

    #[test]
    fn chip_labels_match_immich_wording() {
        assert_eq!(SearchChip::People.label(), "People");
        assert_eq!(SearchChip::Location.label(), "Location");
        assert_eq!(SearchChip::Camera.label(), "Camera");
        assert_eq!(SearchChip::Date.label(), "Date");
        assert_eq!(SearchChip::MediaType.label(), "Media type");
    }

    #[test]
    fn chip_icons_are_symbolic() {
        for chip in SearchChip::all() {
            assert!(
                chip.icon().ends_with("-symbolic"),
                "chip {chip:?} icon {} is not symbolic",
                chip.icon()
            );
        }
    }

    #[test]
    fn chip_order_is_stable() {
        assert_eq!(
            SearchChip::all(),
            [
                SearchChip::People,
                SearchChip::Location,
                SearchChip::Camera,
                SearchChip::Date,
                SearchChip::MediaType,
            ]
        );
    }

    #[test]
    fn quick_link_labels_and_order() {
        let labels: Vec<&str> = QuickLink::all().iter().map(|l| l.label()).collect();
        assert_eq!(
            labels,
            vec!["Recently taken", "Recently added", "Videos", "Favorites"]
        );
    }

    #[test]
    fn quick_link_icons_are_symbolic() {
        for link in QuickLink::all() {
            assert!(
                link.icon().ends_with("-symbolic"),
                "quick link {link:?} icon {} is not symbolic",
                link.icon()
            );
        }
    }
}
