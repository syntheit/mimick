//! Albums landing page — Immich-mobile-style search / filter chips / sort /
//! list-or-grid layout.
//!
//! Top → bottom the view is: a header row (title + "+" create), a search
//! field, a row of three mutually-exclusive filter chips (All / Shared with me
//! / My albums), a controls row (sort button on the left, list/grid view
//! toggle on the right), and finally the album collection rendered either as
//! full-width list rows (default) or a two-column cover grid.
//!
//! All of the interactive state (search text, chip filter, sort mode, view
//! mode) lives on [`AlbumsViewParts`] behind `Rc<Cell<_>>` / `Rc<RefCell<_>>`
//! so every control simply mutates its slot and calls [`render_albums`], which
//! re-projects the cached album list through [`project_albums`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gtk::prelude::*;

use crate::api_client::{LibraryAlbum, ThumbnailSize};
use crate::app_context::AppContext;

pub type AlbumClick = Rc<dyn Fn(&str, String)>;

/// Sort modes available for the albums landing page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AlbumsSort {
    /// Newest created first ("Most recent").
    #[default]
    Newest,
    /// Oldest created first.
    Oldest,
    /// Alphabetical, case-insensitive ("Name A–Z").
    Name,
    /// Largest asset count first ("Most items").
    MostAssets,
}

impl AlbumsSort {
    /// Human label shown on the sort button.
    fn label(self) -> &'static str {
        match self {
            AlbumsSort::Newest => "Most recent",
            AlbumsSort::Oldest => "Oldest",
            AlbumsSort::Name => "Name A–Z",
            AlbumsSort::MostAssets => "Most items",
        }
    }

    /// The next mode when the sort button is tapped (cycles through the four).
    fn next(self) -> AlbumsSort {
        match self {
            AlbumsSort::Newest => AlbumsSort::Oldest,
            AlbumsSort::Oldest => AlbumsSort::Name,
            AlbumsSort::Name => AlbumsSort::MostAssets,
            AlbumsSort::MostAssets => AlbumsSort::Newest,
        }
    }
}

/// Which ownership bucket the filter chips are currently showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ChipFilter {
    /// Owned + shared, merged and sorted ("All").
    #[default]
    All,
    /// Only albums shared with the current user ("Shared with me").
    Shared,
    /// Only albums the current user owns ("My albums").
    Owned,
}

/// List rows (default) versus a two-column cover grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ViewMode {
    #[default]
    List,
    Grid,
}

/// Contains references to the widgets of the albums overview display plus the
/// interactive view state that drives re-rendering.
pub struct AlbumsViewParts {
    pub root: gtk::ScrolledWindow,
    pub populated: Rc<Cell<bool>>,
    pub create_button: gtk::Button,

    /// Search field the user types into to filter albums live.
    search_entry: gtk::SearchEntry,
    /// The three filter chips, in order: All, Shared with me, My albums.
    chip_all: gtk::ToggleButton,
    chip_shared: gtk::ToggleButton,
    chip_owned: gtk::ToggleButton,
    /// Sort button and its label (label text tracks the active sort).
    sort_label: gtk::Label,
    /// View-toggle button and its icon (swaps between list/grid glyphs).
    view_toggle: gtk::Button,
    view_icon: gtk::Image,
    /// Container the rendered rows/tiles are dropped into on every re-render.
    list_container: gtk::Box,
    /// Shown when the active filter yields no albums.
    empty_label: gtk::Label,

    cached_albums: Rc<RefCell<Vec<LibraryAlbum>>>,
    cached_click: Rc<RefCell<Option<AlbumClick>>>,
    cached_ctx: Rc<RefCell<Option<Arc<AppContext>>>>,

    pub search_query: Rc<RefCell<String>>,
    pub sort_mode: Rc<Cell<AlbumsSort>>,
    chip_filter: Rc<Cell<ChipFilter>>,
    view_mode: Rc<Cell<ViewMode>>,
}

/// Construct the search / chips / sort / list-or-grid scaffold for the Albums
/// tab. Interactive state is wired here; actual content arrives via
/// [`populate_albums`].
pub fn build_albums_view() -> AlbumsViewParts {
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .margin_top(12)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();

    // --- Header row: title + "+" create button --------------------------
    let header_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let title = gtk::Label::builder()
        .label("Albums")
        .xalign(0.0)
        .hexpand(true)
        .css_classes(vec!["title-1".to_string()])
        .build();
    let create_button = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Create album")
        .css_classes(vec!["flat".to_string(), "circular".to_string()])
        .valign(gtk::Align::Center)
        .build();
    header_row.append(&title);
    header_row.append(&create_button);
    outer.append(&header_row);

    // --- Search field ---------------------------------------------------
    let search_entry = gtk::SearchEntry::builder()
        .placeholder_text("Search albums")
        .hexpand(true)
        .build();
    outer.append(&search_entry);

    // --- Filter chips ---------------------------------------------------
    let chip_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let chip_all = build_chip("All", true);
    let chip_shared = build_chip("Shared with me", false);
    let chip_owned = build_chip("My albums", false);
    // Group the toggles so exactly one stays active at a time.
    chip_shared.set_group(Some(&chip_all));
    chip_owned.set_group(Some(&chip_all));
    chip_row.append(&chip_all);
    chip_row.append(&chip_shared);
    chip_row.append(&chip_owned);
    outer.append(&chip_row);

    // --- Controls row: sort (left) + view toggle (right) ----------------
    let controls_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();

    let sort_inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();
    let sort_caret = gtk::Image::from_icon_name("pan-down-symbolic");
    let sort_label = gtk::Label::builder()
        .label(AlbumsSort::default().label())
        .css_classes(vec!["caption-heading".to_string()])
        .build();
    sort_inner.append(&sort_caret);
    sort_inner.append(&sort_label);
    let sort_button = gtk::Button::builder()
        .child(&sort_inner)
        .css_classes(vec!["flat".to_string()])
        .halign(gtk::Align::Start)
        .hexpand(true)
        .build();

    let view_icon = gtk::Image::from_icon_name("view-grid-symbolic");
    let view_toggle = gtk::Button::builder()
        .child(&view_icon)
        .tooltip_text("Switch to grid view")
        .css_classes(vec!["flat".to_string(), "circular".to_string()])
        .halign(gtk::Align::End)
        .build();

    controls_row.append(&sort_button);
    controls_row.append(&view_toggle);
    outer.append(&controls_row);

    // --- Album collection container -------------------------------------
    let list_container = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    outer.append(&list_container);

    let empty_label = gtk::Label::builder()
        .label("No albums")
        .css_classes(vec!["dim-label".to_string()])
        .margin_top(24)
        .visible(false)
        .build();
    outer.append(&empty_label);

    let root = gtk::ScrolledWindow::builder()
        .child(&outer)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    let parts = AlbumsViewParts {
        root,
        populated: Rc::new(Cell::new(false)),
        create_button,
        search_entry: search_entry.clone(),
        chip_all: chip_all.clone(),
        chip_shared: chip_shared.clone(),
        chip_owned: chip_owned.clone(),
        sort_label,
        view_toggle: view_toggle.clone(),
        view_icon,
        list_container,
        empty_label,
        cached_albums: Rc::new(RefCell::new(Vec::new())),
        cached_click: Rc::new(RefCell::new(None)),
        cached_ctx: Rc::new(RefCell::new(None)),
        search_query: Rc::new(RefCell::new(String::new())),
        sort_mode: Rc::new(Cell::new(AlbumsSort::default())),
        chip_filter: Rc::new(Cell::new(ChipFilter::default())),
        view_mode: Rc::new(Cell::new(ViewMode::default())),
    };

    wire_controls(&parts, &search_entry, &sort_button, &view_toggle);
    parts
}

/// Attach the live-filtering / chip / sort / view-toggle signal handlers. Each
/// handler mutates the corresponding state slot and re-renders. All the state
/// cells and cache handles are cloned into the closures so the handlers stay
/// alive independently of `parts`.
fn wire_controls(
    parts: &AlbumsViewParts,
    search_entry: &gtk::SearchEntry,
    sort_button: &gtk::Button,
    view_toggle: &gtk::Button,
) {
    // A cheap, cloneable bundle of everything a re-render needs. `render`
    // rebuilds an `AlbumsViewParts`-shaped context is overkill, so instead we
    // hand each closure the individual handles it mutates plus a shared
    // `rerender` trigger built from the widget refs.
    let ctx = RenderCtx::from_parts(parts);

    // Live search filtering.
    {
        let ctx = ctx.clone();
        search_entry.connect_search_changed(move |entry| {
            *ctx.search_query.borrow_mut() = entry.text().to_string();
            ctx.render();
        });
    }

    // Chip selection — the ToggleButtons share a group, so we react to the
    // one becoming active and map it to a filter.
    {
        let ctx = ctx.clone();
        parts.chip_all.connect_toggled(move |btn| {
            if btn.is_active() {
                ctx.chip_filter.set(ChipFilter::All);
                ctx.render();
            }
        });
    }
    {
        let ctx = ctx.clone();
        parts.chip_shared.connect_toggled(move |btn| {
            if btn.is_active() {
                ctx.chip_filter.set(ChipFilter::Shared);
                ctx.render();
            }
        });
    }
    {
        let ctx = ctx.clone();
        parts.chip_owned.connect_toggled(move |btn| {
            if btn.is_active() {
                ctx.chip_filter.set(ChipFilter::Owned);
                ctx.render();
            }
        });
    }

    // Sort button cycles through the four sort modes.
    {
        let ctx = ctx.clone();
        sort_button.connect_clicked(move |_| {
            let next = ctx.sort_mode.get().next();
            ctx.sort_mode.set(next);
            ctx.sort_label.set_label(next.label());
            ctx.render();
        });
    }

    // View toggle flips list <-> grid.
    {
        let ctx = ctx.clone();
        view_toggle.connect_clicked(move |btn| {
            let next = match ctx.view_mode.get() {
                ViewMode::List => ViewMode::Grid,
                ViewMode::Grid => ViewMode::List,
            };
            ctx.view_mode.set(next);
            // Icon shows the mode you would switch *to* (mirrors iOS).
            match next {
                ViewMode::List => {
                    ctx.view_icon.set_icon_name(Some("view-grid-symbolic"));
                    btn.set_tooltip_text(Some("Switch to grid view"));
                }
                ViewMode::Grid => {
                    ctx.view_icon.set_icon_name(Some("view-list-symbolic"));
                    btn.set_tooltip_text(Some("Switch to list view"));
                }
            }
            ctx.render();
        });
    }
}

/// A cloneable snapshot of the handles needed to re-render, so the signal
/// closures don't each have to capture the whole `AlbumsViewParts`.
#[derive(Clone)]
struct RenderCtx {
    populated: Rc<Cell<bool>>,
    list_container: gtk::Box,
    empty_label: gtk::Label,
    sort_label: gtk::Label,
    view_icon: gtk::Image,
    cached_albums: Rc<RefCell<Vec<LibraryAlbum>>>,
    cached_click: Rc<RefCell<Option<AlbumClick>>>,
    cached_ctx: Rc<RefCell<Option<Arc<AppContext>>>>,
    search_query: Rc<RefCell<String>>,
    sort_mode: Rc<Cell<AlbumsSort>>,
    chip_filter: Rc<Cell<ChipFilter>>,
    view_mode: Rc<Cell<ViewMode>>,
}

impl RenderCtx {
    fn from_parts(parts: &AlbumsViewParts) -> Self {
        RenderCtx {
            populated: parts.populated.clone(),
            list_container: parts.list_container.clone(),
            empty_label: parts.empty_label.clone(),
            sort_label: parts.sort_label.clone(),
            view_icon: parts.view_icon.clone(),
            cached_albums: parts.cached_albums.clone(),
            cached_click: parts.cached_click.clone(),
            cached_ctx: parts.cached_ctx.clone(),
            search_query: parts.search_query.clone(),
            sort_mode: parts.sort_mode.clone(),
            chip_filter: parts.chip_filter.clone(),
            view_mode: parts.view_mode.clone(),
        }
    }

    fn render(&self) {
        render_from(
            &self.list_container,
            &self.empty_label,
            &self.populated,
            &self.cached_ctx,
            &self.cached_click,
            &self.cached_albums,
            &self.search_query,
            self.sort_mode.get(),
            self.chip_filter.get(),
            self.view_mode.get(),
        );
    }
}

/// Populate the albums view with data and (re)render the current view.
pub fn populate_albums(
    parts: &AlbumsViewParts,
    ctx: Arc<AppContext>,
    albums: Vec<LibraryAlbum>,
    on_click: AlbumClick,
) {
    *parts.cached_albums.borrow_mut() = albums;
    *parts.cached_click.borrow_mut() = Some(on_click);
    *parts.cached_ctx.borrow_mut() = Some(ctx);
    render_albums(parts);
}

/// Set the current search filter and re-render. Empty string clears the filter.
///
/// Kept for external callers (e.g. tab switches that reset the query); it keeps
/// the in-view search entry in sync so the two never diverge.
pub fn set_search_filter(parts: &AlbumsViewParts, query: &str) {
    *parts.search_query.borrow_mut() = query.to_string();
    // Keep the visible entry consistent without re-triggering `search_changed`
    // recursively (setting the same text is a no-op if already equal).
    if parts.search_entry.text() != query {
        parts.search_entry.set_text(query);
    }
    render_albums(parts);
}

/// Set the current sort mode and re-render.
#[allow(dead_code)]
pub fn set_sort_mode(parts: &AlbumsViewParts, mode: AlbumsSort) {
    parts.sort_mode.set(mode);
    parts.sort_label.set_label(mode.label());
    render_albums(parts);
}

/// Pure projection over the cached album list. Returns the (recent, owned,
/// shared) buckets after applying the active search filter and sort mode.
/// Extracted so the ordering rules can be unit-tested without GTK.
///
/// - `recent`: creation-order (newest first), capped at 8, regardless of `sort`.
/// - `owned`: albums owned by `current_user`, in `sort` order.
/// - `shared`: albums NOT owned by `current_user`, in `sort` order.
///
/// The chip filter maps onto these buckets in [`render_albums`]:
/// All = owned+shared merged and re-sorted, Shared = `shared`, Owned = `owned`.
pub(crate) fn project_albums<'a>(
    albums: &'a [LibraryAlbum],
    current_user: &str,
    query: &str,
    sort: AlbumsSort,
) -> (
    Vec<&'a LibraryAlbum>,
    Vec<&'a LibraryAlbum>,
    Vec<&'a LibraryAlbum>,
) {
    let query = query.to_ascii_lowercase();
    let filtered: Vec<&LibraryAlbum> = albums
        .iter()
        .filter(|a| query.is_empty() || a.album_name.to_ascii_lowercase().contains(&query))
        .collect();

    let mut sorted: Vec<&LibraryAlbum> = filtered.clone();
    sort_albums(&mut sorted, sort);

    // The "Recent" bucket always reflects creation order, even when the user
    // picks a different sort for the main owned/shared rows.
    let recent: Vec<&LibraryAlbum> = if matches!(sort, AlbumsSort::Newest) {
        sorted.iter().copied().take(8).collect()
    } else {
        let mut by_date: Vec<&LibraryAlbum> = filtered.clone();
        by_date.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        by_date.into_iter().take(8).collect()
    };

    let owned: Vec<&LibraryAlbum> = sorted
        .iter()
        .copied()
        .filter(|a| !current_user.is_empty() && a.owner_id() == current_user)
        .collect();
    let shared: Vec<&LibraryAlbum> = sorted
        .iter()
        .copied()
        .filter(|a| !current_user.is_empty() && a.owner_id() != current_user)
        .collect();

    (recent, owned, shared)
}

/// Sort a slice of album refs in place according to `sort`. Shared by
/// `project_albums` and the "All" chip merge so ordering is identical.
fn sort_albums(list: &mut [&LibraryAlbum], sort: AlbumsSort) {
    match sort {
        AlbumsSort::Newest => list.sort_by(|a, b| b.created_at.cmp(&a.created_at)),
        AlbumsSort::Oldest => list.sort_by(|a, b| a.created_at.cmp(&b.created_at)),
        AlbumsSort::Name => list.sort_by(|a, b| {
            a.album_name
                .to_ascii_lowercase()
                .cmp(&b.album_name.to_ascii_lowercase())
        }),
        AlbumsSort::MostAssets => list.sort_by_key(|a| std::cmp::Reverse(a.asset_count)),
    }
}

/// Re-render using the state currently stored on `parts`.
fn render_albums(parts: &AlbumsViewParts) {
    render_from(
        &parts.list_container,
        &parts.empty_label,
        &parts.populated,
        &parts.cached_ctx,
        &parts.cached_click,
        &parts.cached_albums,
        &parts.search_query,
        parts.sort_mode.get(),
        parts.chip_filter.get(),
        parts.view_mode.get(),
    );
}

/// The single render path shared by `render_albums` and the signal closures.
/// Clears the container, projects the cached albums through the active filter
/// state, and rebuilds either list rows or grid tiles.
#[allow(clippy::too_many_arguments)]
fn render_from(
    list_container: &gtk::Box,
    empty_label: &gtk::Label,
    populated: &Rc<Cell<bool>>,
    cached_ctx: &Rc<RefCell<Option<Arc<AppContext>>>>,
    cached_click: &Rc<RefCell<Option<AlbumClick>>>,
    cached_albums: &Rc<RefCell<Vec<LibraryAlbum>>>,
    search_query: &Rc<RefCell<String>>,
    sort: AlbumsSort,
    chip: ChipFilter,
    view: ViewMode,
) {
    let ctx_opt = cached_ctx.borrow().clone();
    let on_click_opt = cached_click.borrow().clone();
    let (Some(ctx), Some(on_click)) = (ctx_opt, on_click_opt) else {
        return;
    };

    clear_box(list_container);

    let current_user = ctx.current_user_id.lock().clone().unwrap_or_default();
    let query = search_query.borrow().clone();

    let albums = cached_albums.borrow();
    let (_recent, owned, shared) = project_albums(&albums, &current_user, &query, sort);

    // Map the chip filter onto the projected buckets.
    let visible: Vec<&LibraryAlbum> = match chip {
        ChipFilter::Owned => owned,
        ChipFilter::Shared => shared,
        ChipFilter::All => {
            let mut all: Vec<&LibraryAlbum> = owned.into_iter().chain(shared).collect();
            sort_albums(&mut all, sort);
            all
        }
    };

    if visible.is_empty() {
        empty_label.set_visible(true);
        list_container.set_visible(false);
        populated.set(true);
        return;
    }
    empty_label.set_visible(false);
    list_container.set_visible(true);

    match view {
        ViewMode::List => {
            let list = gtk::ListBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .css_classes(vec!["boxed-list".to_string()])
                .build();
            for album in &visible {
                list.append(&album_row(ctx.clone(), album, &current_user, on_click.clone()));
            }
            list_container.append(&list);
        }
        ViewMode::Grid => {
            let grid = gtk::FlowBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .max_children_per_line(2)
                .min_children_per_line(2)
                .row_spacing(12)
                .column_spacing(12)
                .homogeneous(true)
                .build();
            for album in &visible {
                grid.insert(&album_tile(ctx.clone(), album, on_click.clone()), -1);
            }
            list_container.append(&grid);
        }
    }

    populated.set(true);
}

/// Build a filter chip (rounded pill toggle). The `selected` chip starts
/// active; the caller groups them so exactly one is active at a time.
fn build_chip(label: &str, selected: bool) -> gtk::ToggleButton {
    gtk::ToggleButton::builder()
        .label(label)
        .active(selected)
        .css_classes(vec!["pill".to_string()])
        .build()
}

/// Build a single full-width album list row: a ~56px rounded cover thumbnail,
/// then the album name and a "N items • Owned/Shared" subtitle. The whole row
/// is a flat button so tapping anywhere navigates into the album.
fn album_row(
    ctx: Arc<AppContext>,
    album: &LibraryAlbum,
    current_user: &str,
    on_click: AlbumClick,
) -> gtk::Widget {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(4)
        .margin_bottom(4)
        .build();

    // Fixed 56x56 rounded thumbnail (Overlay so a portrait cover can't inflate
    // the row height — the spacer sets the size, the picture covers it). Reuse
    // `mimick-explore-tile` for the rounded corners + placeholder fill; the
    // 56px size is requested directly since we can't add new CSS here.
    let thumb = gtk::Overlay::builder()
        .overflow(gtk::Overflow::Hidden)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .css_classes(vec!["mimick-explore-tile".to_string()])
        .build();
    let spacer = gtk::Box::builder().build();
    spacer.set_size_request(56, 56);
    let picture = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Cover)
        .build();
    thumb.set_child(Some(&spacer));
    thumb.add_overlay(&picture);

    let text_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    let title_label = gtk::Label::builder()
        .label(&album.album_name)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(1)
        .css_classes(vec!["heading".to_string()])
        .build();
    let ownership = if !current_user.is_empty() && album.owner_id() == current_user {
        "Owned"
    } else {
        "Shared"
    };
    let subtitle = gtk::Label::builder()
        .label(format!(
            "{} item{} • {}",
            album.asset_count,
            if album.asset_count == 1 { "" } else { "s" },
            ownership
        ))
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
        .build();
    text_box.append(&title_label);
    text_box.append(&subtitle);

    row.append(&thumb);
    row.append(&text_box);

    if let Some(thumb_id) = album.thumbnail_asset_id.clone() {
        spawn_thumbnail(ctx, thumb_id, picture);
    }

    // Wrap in a flat button so the whole row is tappable.
    let button = gtk::Button::builder()
        .child(&row)
        .css_classes(vec!["flat".to_string()])
        .build();
    let id = album.id.clone();
    let name = album.album_name.clone();
    button.connect_clicked(move |_| on_click(&id, name.clone()));
    button.upcast()
}

/// Build a clickable grid tile representing an individual album with cover art
/// (reuses the explore-tile look). Used by the grid view mode.
fn album_tile(ctx: Arc<AppContext>, album: &LibraryAlbum, on_click: AlbumClick) -> gtk::Button {
    let tile_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();

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

    let meta_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let title_label = gtk::Label::builder()
        .label(&album.album_name)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .width_chars(1)
        .max_width_chars(1)
        .css_classes(vec!["caption-heading".to_string()])
        .build();
    let count_label = gtk::Label::builder()
        .label(format!(
            "{} item{}",
            album.asset_count,
            if album.asset_count == 1 { "" } else { "s" }
        ))
        .xalign(1.0)
        .css_classes(vec!["caption".to_string(), "dim-label".to_string()])
        .build();
    meta_row.append(&title_label);
    meta_row.append(&count_label);
    tile_box.append(&thumb);
    tile_box.append(&meta_row);

    let button = gtk::Button::builder()
        .child(&tile_box)
        .css_classes(vec!["flat".to_string()])
        .build();

    if let Some(thumb_id) = album.thumbnail_asset_id.clone() {
        spawn_thumbnail(ctx, thumb_id, picture);
    }

    let id = album.id.clone();
    let name = album.album_name.clone();
    button.connect_clicked(move |_| on_click(&id, name.clone()));
    button
}

/// Asynchronously load and set the thumbnail for an album cover art picture widget.
fn spawn_thumbnail(ctx: Arc<AppContext>, asset_id: String, picture: gtk::Picture) {
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

/// Remove all child widgets from a box container.
fn clear_box(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::api_client::{AlbumUser, AlbumUserInfo};

    fn album(id: &str, name: &str, owner: &str, count: u32, created: &str) -> LibraryAlbum {
        LibraryAlbum {
            id: id.into(),
            album_name: name.into(),
            asset_count: count,
            thumbnail_asset_id: None,
            created_at: created.into(),
            updated_at: created.into(),
            description: String::new(),
            album_users: vec![AlbumUser {
                user: AlbumUserInfo { id: owner.into() },
                role: "owner".into(),
            }],
        }
    }

    fn fixture() -> Vec<LibraryAlbum> {
        vec![
            album("a1", "Beach Trip", "me", 200, "2024-06-01T00:00:00Z"),
            album("a2", "Family", "friend", 50, "2025-01-10T00:00:00Z"),
            album("a3", "Archive 2020", "me", 1000, "2020-12-31T00:00:00Z"),
            album("a4", "beach-volleyball", "me", 12, "2023-09-15T00:00:00Z"),
        ]
    }

    #[test]
    fn project_filters_by_case_insensitive_substring() {
        let items = fixture();
        let (_, owned, _) = project_albums(&items, "me", "beach", AlbumsSort::Name);
        let names: Vec<&str> = owned.iter().map(|a| a.album_name.as_str()).collect();
        assert_eq!(names, vec!["Beach Trip", "beach-volleyball"]);
    }

    #[test]
    fn project_sort_newest_orders_by_created_desc() {
        let items = fixture();
        let (_, owned, _) = project_albums(&items, "me", "", AlbumsSort::Newest);
        let ids: Vec<&str> = owned.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["a1", "a4", "a3"]);
    }

    #[test]
    fn project_sort_name_is_case_insensitive() {
        let items = fixture();
        let (_, owned, _) = project_albums(&items, "me", "", AlbumsSort::Name);
        let names: Vec<&str> = owned.iter().map(|a| a.album_name.as_str()).collect();
        // "Archive 2020", "Beach Trip", "beach-volleyball" — lowercase-sorted.
        assert_eq!(
            names,
            vec!["Archive 2020", "Beach Trip", "beach-volleyball"]
        );
    }

    #[test]
    fn project_sort_most_assets_descends() {
        let items = fixture();
        let (_, owned, _) = project_albums(&items, "me", "", AlbumsSort::MostAssets);
        let counts: Vec<u32> = owned.iter().map(|a| a.asset_count).collect();
        assert_eq!(counts, vec![1000, 200, 12]);
    }

    #[test]
    fn project_buckets_by_owner() {
        let items = fixture();
        let (_, owned, shared) = project_albums(&items, "me", "", AlbumsSort::Newest);
        assert_eq!(owned.len(), 3);
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].id, "a2");
    }

    #[test]
    fn project_empty_current_user_excludes_from_both_buckets() {
        // Without a known user id we can't safely classify owner vs shared,
        // so both buckets stay empty to avoid mis-labeling.
        let items = fixture();
        let (recent, owned, shared) = project_albums(&items, "", "", AlbumsSort::Newest);
        assert!(owned.is_empty());
        assert!(shared.is_empty());
        // Recent still populates regardless of user identity.
        assert_eq!(recent.len(), 4);
    }

    #[test]
    fn project_recent_always_by_date_even_under_alternate_sort() {
        let items = fixture();
        let (recent, _, _) = project_albums(&items, "me", "", AlbumsSort::MostAssets);
        let ids: Vec<&str> = recent.iter().map(|a| a.id.as_str()).collect();
        // Recent is creation-order, not asset-count, so the "Family" album
        // (newest) leads even though it's not the largest.
        assert_eq!(ids[0], "a2");
    }

    #[test]
    fn project_recent_caps_at_eight() {
        let mut items = Vec::new();
        for i in 0..20 {
            items.push(album(
                &format!("id{i}"),
                &format!("Album {i}"),
                "me",
                i,
                &format!("2024-01-{:02}T00:00:00Z", i + 1),
            ));
        }
        let (recent, _, _) = project_albums(&items, "me", "", AlbumsSort::Newest);
        assert_eq!(recent.len(), 8);
    }

    #[test]
    fn project_sort_oldest_orders_by_created_asc() {
        let items = fixture();
        let (_, owned, _) = project_albums(&items, "me", "", AlbumsSort::Oldest);
        let ids: Vec<&str> = owned.iter().map(|a| a.id.as_str()).collect();
        // Oldest → newest among owned: a3 (2020), a4 (2023), a1 (2024).
        assert_eq!(ids, vec!["a3", "a4", "a1"]);
    }
}
