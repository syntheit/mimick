//! Bottom-nav shell scaffolding for the library window.
//!
//! Immich-mobile-style shell: an [`libadwaita::ViewStack`] with four pages
//! (Photos / Search / Albums / Library), each wrapping its own
//! [`libadwaita::NavigationView`] so drill-in detail pages get swipe-back.
//! A header [`libadwaita::ViewSwitcher`] (wide) and a bottom
//! [`libadwaita::ViewSwitcherBar`] (narrow) drive the top-level tabs.
//!
//! The photos grid canvas is a single widget (`GridViewParts`), so drill-in
//! detail grids (album contents, filtered people/places results) *reparent*
//! that one canvas into a pushed [`libadwaita::NavigationPage`] and reparent
//! it back to the Photos tab when the page is popped. This keeps the lightbox
//! (which reads `ui.grid.model` and pushes onto the root `ui.nav`) working
//! uniformly regardless of which tab the viewer was opened from.

use gtk::prelude::*;

/// The four top-level tabs, matching the Immich mobile bottom nav order.
pub const TAB_PHOTOS: &str = "photos";
pub const TAB_SEARCH: &str = "search";
pub const TAB_ALBUMS: &str = "albums";
pub const TAB_LIBRARY: &str = "library";

/// A single top-level tab: its [`NavigationView`] plus the small
/// loading/empty/error/content stack that lives on the tab's root page.
///
/// `stack` children are named `loading`, `empty`, `error`, and `content`.
/// The `content` slot is filled by the caller with the tab's real view
/// (the photos grid, the albums landing, the explore landing, …).
#[derive(Clone)]
pub struct TabView {
    pub nav: libadwaita::NavigationView,
    pub stack: gtk::Stack,
    pub error_label: gtk::Label,
    /// The `content` child of `stack`; caller-owned real view.
    pub content_slot: gtk::Box,
}

impl TabView {
    /// Build a tab whose root page hosts a loading/empty/error/content stack.
    /// `title` names the root [`NavigationPage`]. The returned tab starts on
    /// the `loading` child; use [`Self::show_content`] once data is bound.
    pub fn new(title: &str) -> Self {
        let stack = gtk::Stack::builder()
            .vexpand(true)
            .hexpand(true)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(160)
            .build();

        let loading = super::build_loading_view();
        let empty = super::build_status_view(
            "image-x-generic-symbolic",
            "Nothing to show",
            "No assets match the current view",
        );
        let error = super::build_status_view(
            "dialog-warning-symbolic",
            "Library data unavailable",
            "Could not load library assets",
        );
        let error_label = error
            .last_child()
            .and_downcast::<gtk::Label>()
            .expect("status-view subtitle label");

        // The content slot is an empty box the caller fills with the real view.
        let content_slot = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .vexpand(true)
            .hexpand(true)
            .build();

        stack.add_named(&loading, Some("loading"));
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&error, Some("error"));
        stack.add_named(&content_slot, Some("content"));
        stack.set_visible_child_name("loading");

        let nav = libadwaita::NavigationView::new();
        let root = libadwaita::NavigationPage::builder()
            .child(&stack)
            .title(title)
            .can_pop(false)
            .build();
        nav.add(&root);

        Self {
            nav,
            stack,
            error_label,
            content_slot,
        }
    }

    /// Insert the tab's real view into the `content` slot (call once).
    pub fn set_content_child(&self, child: &impl IsA<gtk::Widget>) {
        // Detach any prior child first (idempotent for re-parenting).
        if let Some(existing) = self.content_slot.first_child() {
            self.content_slot.remove(&existing);
        }
        self.content_slot.append(child);
    }

    pub fn show_loading(&self) {
        self.stack.set_visible_child_name("loading");
    }

    pub fn show_content(&self) {
        self.stack.set_visible_child_name("content");
    }

    pub fn show_empty(&self) {
        self.stack.set_visible_child_name("empty");
    }

    pub fn show_error(&self, msg: &str) {
        self.error_label.set_label(msg);
        self.stack.set_visible_child_name("error");
    }

    pub fn visible_child_name(&self) -> Option<glib::GString> {
        self.stack.visible_child_name()
    }
}

/// A pushed drill-in detail page (album contents / filtered results).
///
/// Wraps a small loading/empty/error/content stack in a
/// [`NavigationPage`]. The `content` slot receives the *shared* photos-grid
/// scrolled window on push and releases it on pop, so the lightbox keeps
/// reading the one `ui.grid.model`.
#[derive(Clone)]
pub struct DrillPage {
    pub page: libadwaita::NavigationPage,
    pub stack: gtk::Stack,
    pub error_label: gtk::Label,
    pub content_slot: gtk::Box,
}

impl DrillPage {
    pub fn new(title: &str) -> Self {
        let stack = gtk::Stack::builder()
            .vexpand(true)
            .hexpand(true)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(160)
            .build();

        let loading = super::build_loading_view();
        let empty = super::build_status_view(
            "image-x-generic-symbolic",
            "Nothing to show",
            "No assets match the current view",
        );
        let error = super::build_status_view(
            "dialog-warning-symbolic",
            "Library data unavailable",
            "Could not load library assets",
        );
        let error_label = error
            .last_child()
            .and_downcast::<gtk::Label>()
            .expect("status-view subtitle label");

        let content_slot = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .vexpand(true)
            .hexpand(true)
            .build();

        stack.add_named(&loading, Some("loading"));
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&error, Some("error"));
        stack.add_named(&content_slot, Some("content"));
        stack.set_visible_child_name("loading");

        let toolbar = libadwaita::ToolbarView::builder().build();
        toolbar.add_top_bar(&libadwaita::HeaderBar::new());
        toolbar.set_content(Some(&stack));

        let page = libadwaita::NavigationPage::builder()
            .child(&toolbar)
            .title(title)
            .build();

        Self {
            page,
            stack,
            error_label,
            content_slot,
        }
    }

    pub fn show_loading(&self) {
        self.stack.set_visible_child_name("loading");
    }
    pub fn show_content(&self) {
        self.stack.set_visible_child_name("content");
    }
    pub fn show_empty(&self) {
        self.stack.set_visible_child_name("empty");
    }
    pub fn show_error(&self, msg: &str) {
        self.error_label.set_label(msg);
        self.stack.set_visible_child_name("error");
    }
}

/// Detach `child` from its current parent, if any. Needed before re-parenting
/// the shared grid scrolled window between the Photos tab and a drill-in page.
pub fn unparent_from_slot(child: &impl IsA<gtk::Widget>) {
    if let Some(parent) = child.parent() {
        if let Some(bx) = parent.downcast_ref::<gtk::Box>() {
            bx.remove(child);
        } else if let Some(stack) = parent.downcast_ref::<gtk::Stack>() {
            stack.remove(child);
        } else {
            child.unparent();
        }
    }
}
