//! Full-screen, browsable "Places" map of where the user's photos were taken.
//!
//! Fetches every geotagged asset from Immich's `/api/map/markers` endpoint and
//! drops pins on an OpenStreetMap raster map (libshumate's `SimpleMap`, the same
//! tile setup the per-photo lightbox minimap uses). On open the viewport is
//! fitted to the markers' bounding box so the user sees the whole spread of
//! their travels; tapping a pin opens that asset in the lightbox.
//!
//! Because libshumate has no built-in clustering and a large library can return
//! tens of thousands of markers (rendering every one as a widget would jank),
//! markers are **grid-clustered client-side**: coordinates are rounded to a grid
//! whose cell size shrinks as you zoom in, and each occupied cell becomes one
//! pin. A cell with a single asset is an asset pin (tap → lightbox); a cell with
//! several is a cluster pin showing the count (tap → zoom in on it). The layer
//! is rebuilt whenever the map's zoom level changes so clusters split apart as
//! you zoom.

use std::rc::Rc;

use glib::clone;
use gtk::prelude::*;
use libadwaita::prelude::*;
use libshumate::prelude::*;

use crate::api_client::MapMarker;

use super::{LibraryWindowUi, open_asset_in_lightbox};

/// Hard cap on how many *pins* (clusters or asset markers) we ever add to the
/// layer in one pass. Grid-clustering already collapses dense areas to a single
/// pin per cell, so in practice we stay far below this; the cap is a last-resort
/// guard so a pathological spread (markers scattered so every grid cell holds
/// exactly one) can never spawn tens of thousands of widgets and jank the UI.
const MAX_PINS: usize = 2_000;

/// Fetch the map markers and present the full-screen Places map page.
///
/// Pushes onto the *root* nav (`ui.nav`) so the map covers the bottom tab bar
/// like the lightbox does. The marker fetch is async; the page is built and
/// pushed immediately (showing a spinner), and populated when markers arrive.
/// If the page has already been popped by the time the fetch returns, the
/// update is skipped (guarded via a weak reference to the page).
pub(super) fn open_places_map(ui: Rc<LibraryWindowUi>) {
    // ── Build the map + page shell up front (spinner state) ─────────────
    let simple_map = build_simple_map();
    // A stack swaps between the loading spinner, the live map, and the empty
    // state so we never show a blank grey map while markers are in flight.
    let stack = gtk::Stack::builder().vexpand(true).hexpand(true).build();

    let spinner = gtk::Spinner::builder()
        .width_request(32)
        .height_request(32)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    spinner.start();
    let loading = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    loading.append(&spinner);
    stack.add_named(&loading, Some("loading"));
    stack.add_named(&empty_state(), Some("empty"));
    stack.add_named(&simple_map, Some("map"));
    stack.set_visible_child_name("loading");

    // Page chrome mirrors backup_view: a HeaderBar in a ToolbarView. The
    // NavigationPage title auto-displays in the HeaderBar, and AdwNavigationView
    // supplies the back button automatically when the page is pushed.
    let header = libadwaita::HeaderBar::new();
    let toolbar = libadwaita::ToolbarView::builder().build();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));

    let page = libadwaita::NavigationPage::builder()
        .title("Map")
        .child(&toolbar)
        .build();

    ui.nav.push(&page);

    // ── Fetch markers, then populate (guarded against a popped page) ─────
    let page_weak = page.downgrade();
    glib::MainContext::default().spawn_local(clone!(
        #[strong]
        ui,
        #[strong]
        simple_map,
        #[strong]
        stack,
        async move {
            let markers = match ui.ctx.api_client.fetch_map_markers().await {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("Places map: failed to fetch markers: {e}");
                    Vec::new()
                }
            };
            // If the page was popped while we were fetching, do nothing (the
            // widgets are being torn down / may already be gone).
            if page_weak.upgrade().is_none() {
                return;
            }
            if markers.is_empty() {
                stack.set_visible_child_name("empty");
                return;
            }
            populate_map(&ui, &simple_map, markers);
            stack.set_visible_child_name("map");
        }
    ));
}

/// Build a `SimpleMap` with the OSM Mapnik raster tile source.
///
/// This copies the tile-source setup from the lightbox's `build_minimap`
/// verbatim (the tricky part — the `RasterRenderer::new_full_from_url` layout
/// must match exactly for tiles to load), sized to fill the page instead of the
/// 180px minimap.
///
/// Returned `vexpand`/`hexpand` are both `true`; the embedded Places map in
/// `explore_view` overrides `vexpand` to `false` and wraps it in a fixed-height
/// host so it can't inflate the page scroll.
pub(super) fn build_simple_map() -> libshumate::SimpleMap {
    let map = libshumate::SimpleMap::new();
    map.set_vexpand(true);
    map.set_hexpand(true);

    // OSM raster tiles. Immich uses the standard Mapnik tile server layout.
    let source = libshumate::RasterRenderer::new_full_from_url(
        "osm-mapnik",
        "OpenStreetMap",
        "© OpenStreetMap contributors",
        "https://www.openstreetmap.org/copyright",
        0,
        19,
        256,
        libshumate::MapProjection::Mercator,
        "https://tile.openstreetmap.org/{z}/{x}/{y}.png",
    );
    map.set_map_source(Some(&source));
    map
}

/// A friendly empty state shown when the library has no geotagged assets.
///
/// Shared by the full-screen Places map and the embedded Places map on the
/// Library landing; sized to read fine both full-page and inside the embedded
/// map's fixed-height host.
pub(super) fn empty_state() -> gtk::Widget {
    let icon = gtk::Image::builder()
        .icon_name("mark-location-symbolic")
        .pixel_size(64)
        .css_classes(["dim-label"])
        .build();
    let title = gtk::Label::builder()
        .label("No places yet")
        .css_classes(["title-2"])
        .build();
    let body = gtk::Label::builder()
        .label("Photos with location data will appear on the map here.")
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["dim-label"])
        .build();
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .margin_start(24)
        .margin_end(24)
        .build();
    column.append(&icon);
    column.append(&title);
    column.append(&body);
    column.upcast()
}

/// Populate the map: fit the viewport to the markers' bounding box, add a
/// marker layer, do an initial cluster pass, and rebuild the clustering
/// whenever the zoom level changes.
///
/// Shared by the full-screen Places map ([`open_places_map`]) and the embedded
/// Places map on the Library landing. Each call installs one marker layer and
/// one `zoom-level` notify handler on the viewport, so callers must invoke it
/// at most once per `SimpleMap` instance (the embedded map guards this with a
/// `places_map_populated` flag; the full-screen map is single-use and torn
/// down on pop).
pub(super) fn populate_map(ui: &Rc<LibraryWindowUi>, map: &libshumate::SimpleMap, markers: Vec<MapMarker>) {
    let Some(viewport) = map.viewport() else {
        log::warn!("Places map: SimpleMap has no viewport");
        return;
    };

    // ── Fit the viewport to the bounding box of all markers ─────────────
    let (min_lat, max_lat, min_lon, max_lon) = bounding_box(&markers);
    let center_lat = (min_lat + max_lat) / 2.0;
    let center_lon = (min_lon + max_lon) / 2.0;
    let fit_zoom = zoom_for_span(max_lat - min_lat, max_lon - min_lon);
    // `go_to_full` on the underlying Map jumps to a center + zoom in one call;
    // prefer it over poking the viewport so the initial framing is applied
    // atomically. Fall back to the viewport if the Map isn't ready yet.
    if let Some(inner) = map.map() {
        inner.set_go_to_duration(0);
        inner.go_to_full(center_lat, center_lon, fit_zoom);
    } else {
        viewport.set_zoom_level(fit_zoom);
        viewport.set_location(center_lat, center_lon);
    }

    // ── Marker layer (rebuilt on every cluster pass) ────────────────────
    let layer = libshumate::MarkerLayer::new_full(&viewport, gtk::SelectionMode::None);
    map.add_overlay_layer(&layer);

    let markers = Rc::new(markers);

    // Initial cluster pass at the fit zoom.
    rebuild_clusters(ui, map, &viewport, &layer, &markers, fit_zoom);

    // Re-cluster whenever the *integer* zoom bucket changes so dense clusters
    // split apart as the user zooms in (and merge back on zoom-out). The
    // `zoom-level` property is a continuous f64 that notifies many times during a
    // single pinch/scroll gesture; since the cluster grid granularity is keyed to
    // the integer zoom (`cell_size_for_zoom`), rebuilding on every fractional
    // step would thrash the widget tree for no visible change. We therefore only
    // rebuild when the rounded zoom bucket actually crosses a boundary.
    //
    // The layer/map are held by weak refs so this closure can't keep the popped
    // page's widgets alive (and so the viewport → closure → viewport ownership
    // doesn't form a cycle); the notifying `vp` argument is the live viewport.
    let layer_weak = layer.downgrade();
    let map_weak = map.downgrade();
    let ui = ui.clone();
    let last_bucket = std::cell::Cell::new(fit_zoom.round() as i32);
    viewport.connect_zoom_level_notify(move |vp| {
        let bucket = vp.zoom_level().round() as i32;
        if bucket == last_bucket.get() {
            return;
        }
        last_bucket.set(bucket);
        let (Some(layer), Some(map)) = (layer_weak.upgrade(), map_weak.upgrade()) else {
            return;
        };
        rebuild_clusters(&ui, &map, vp, &layer, &markers, vp.zoom_level());
    });
}

/// Clear and repopulate the marker layer, grid-clustering `markers` at the
/// current `zoom`. Each occupied grid cell yields one pin: a single-asset cell
/// becomes a tappable asset pin (→ lightbox); a multi-asset cell becomes a
/// count pin (→ zoom in). Grid-clustering keeps the pin count low even for huge
/// libraries, so rebuilding the whole layer on each zoom change is cheap; the
/// `viewport::zoom-level` notify that drives it only fires on real zoom deltas.
fn rebuild_clusters(
    ui: &Rc<LibraryWindowUi>,
    map: &libshumate::SimpleMap,
    viewport: &libshumate::Viewport,
    layer: &libshumate::MarkerLayer,
    markers: &[MapMarker],
    zoom: f64,
) {
    layer.remove_all();

    // Grid cell size in degrees, tied to zoom: coarse when zoomed out (so far
    // apart pins merge), fine when zoomed in (so nearby pins separate). At the
    // OSM max zoom (~19) the cell is tiny, so individual photos stand alone.
    let cell = cell_size_for_zoom(zoom);

    // Bucket markers into grid cells. Key is the integer (row, col) of the cell;
    // we keep a running centroid sum + one representative asset id per cell.
    use std::collections::HashMap;
    struct Cell {
        lat_sum: f64,
        lon_sum: f64,
        count: usize,
        sample_id: String,
    }
    let mut cells: HashMap<(i64, i64), Cell> = HashMap::new();
    for m in markers {
        // Skip obviously-invalid coordinates (0,0 null-island / out of range).
        if !m.lat.is_finite() || !m.lon.is_finite() || m.lat.abs() > 90.0 || m.lon.abs() > 180.0 {
            continue;
        }
        let key = ((m.lat / cell).floor() as i64, (m.lon / cell).floor() as i64);
        let entry = cells.entry(key).or_insert_with(|| Cell {
            lat_sum: 0.0,
            lon_sum: 0.0,
            count: 0,
            sample_id: m.id.clone(),
        });
        entry.lat_sum += m.lat;
        entry.lon_sum += m.lon;
        entry.count += 1;
    }

    // Add one pin per cell, capped. Sort by descending count so the densest
    // (most useful) clusters win the cap if we ever hit it.
    let mut buckets: Vec<Cell> = cells.into_values().collect();
    buckets.sort_by(|a, b| b.count.cmp(&a.count));
    if buckets.len() > MAX_PINS {
        log::debug!(
            "Places map: {} grid cells at zoom {:.1}, capping pins to {}",
            buckets.len(),
            zoom,
            MAX_PINS
        );
        buckets.truncate(MAX_PINS);
    }

    for bucket in buckets {
        let lat = bucket.lat_sum / bucket.count as f64;
        let lon = bucket.lon_sum / bucket.count as f64;
        let marker = if bucket.count == 1 {
            asset_pin(ui, &bucket.sample_id)
        } else {
            cluster_pin(map, viewport, bucket.count, lat, lon)
        };
        marker.set_location(lat, lon);
        layer.add_marker(&marker);
    }
}

/// A single-asset pin: a location marker that opens the asset in the lightbox
/// when tapped.
fn asset_pin(ui: &Rc<LibraryWindowUi>, asset_id: &str) -> libshumate::Marker {
    let marker = libshumate::Marker::new();
    let pin = gtk::Image::from_icon_name("mark-location-symbolic");
    pin.set_pixel_size(28);
    pin.add_css_class("error");
    marker.set_child(Some(&pin));

    // Tap → open the asset. A GestureClick on the marker's child widget lets us
    // route the tap without relying on MarkerLayer selection.
    let click = gtk::GestureClick::new();
    let ui = ui.clone();
    let asset_id = asset_id.to_string();
    click.connect_released(move |gesture, _n, _x, _y| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        open_asset_in_lightbox(ui.clone(), asset_id.clone());
    });
    pin.add_controller(click);
    marker
}

/// A cluster pin: a circular badge showing the asset count. Tapping it zooms
/// the map in and recenters on the cluster so it splits into finer pins.
fn cluster_pin(
    map: &libshumate::SimpleMap,
    viewport: &libshumate::Viewport,
    count: usize,
    lat: f64,
    lon: f64,
) -> libshumate::Marker {
    let marker = libshumate::Marker::new();
    let label = gtk::Label::new(Some(&format_count(count)));
    label.add_css_class("mimick-map-cluster");
    marker.set_child(Some(&label));

    let click = gtk::GestureClick::new();
    let map_weak = map.downgrade();
    let viewport_weak = viewport.downgrade();
    click.connect_released(move |gesture, _n, _x, _y| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let Some(viewport) = viewport_weak.upgrade() else {
            return;
        };
        // Zoom in a couple of steps (clamped to the OSM max) and recenter.
        let target = (viewport.zoom_level() + 2.0).min(19.0);
        if let Some(inner) = map_weak.upgrade().and_then(|m| m.map()) {
            inner.go_to_full(lat, lon, target);
        } else {
            viewport.set_zoom_level(target);
            viewport.set_location(lat, lon);
        }
    });
    label.add_controller(click);
    marker
}

/// Compact cluster-count text: exact up to 99, then "99+".
fn format_count(count: usize) -> String {
    if count > 99 {
        "99+".to_string()
    } else {
        count.to_string()
    }
}

/// Compute the lat/lon bounding box of all markers, ignoring invalid coords.
/// Falls back to a whole-world box if nothing is valid (defensive; callers
/// only reach here with a non-empty marker list).
fn bounding_box(markers: &[MapMarker]) -> (f64, f64, f64, f64) {
    let mut min_lat = f64::MAX;
    let mut max_lat = f64::MIN;
    let mut min_lon = f64::MAX;
    let mut max_lon = f64::MIN;
    let mut any = false;
    for m in markers {
        if !m.lat.is_finite() || !m.lon.is_finite() || m.lat.abs() > 90.0 || m.lon.abs() > 180.0 {
            continue;
        }
        any = true;
        min_lat = min_lat.min(m.lat);
        max_lat = max_lat.max(m.lat);
        min_lon = min_lon.min(m.lon);
        max_lon = max_lon.max(m.lon);
    }
    if !any {
        return (-60.0, 70.0, -170.0, 170.0);
    }
    (min_lat, max_lat, min_lon, max_lon)
}

/// Pick a slippy-map zoom level that frames a lat/lon span. Uses the larger of
/// the two spans (in degrees) and the standard tile-zoom relationship
/// (each zoom level halves the visible degree span, and the whole 360° world
/// spans ~one tile at zoom 0). Clamped to the OSM 3..=17 range so a single-point
/// library isn't zoomed to street level and a global one isn't clipped.
fn zoom_for_span(lat_span: f64, lon_span: f64) -> f64 {
    // Largest span drives the fit; guard the degenerate zero-span (all photos
    // at one spot) so we don't take log2(0).
    let span = lat_span.max(lon_span).max(0.0001);
    // 360° ≈ zoom 0 across the map width; each level doubles the resolution.
    // Subtract a margin (1.2) so markers aren't jammed against the edges.
    let zoom = (360.0 / span).log2() - 1.2;
    zoom.clamp(3.0, 17.0)
}

/// Grid cell size (in degrees) used for clustering at a given zoom level. Larger
/// cells at low zoom merge far-apart photos into one pin; the cell shrinks
/// geometrically as you zoom in until, near max zoom, individual photos stand
/// alone. Tuned so a city-level zoom (~11-13) separates neighbourhoods.
fn cell_size_for_zoom(zoom: f64) -> f64 {
    // At zoom 0 the whole world is one screen; a ~1/4-screen cell ≈ 90°. Each
    // zoom step halves it. Floor keeps the grid from collapsing to zero at very
    // high zoom (which would make the HashMap key overflow / cluster nothing).
    let z = zoom.clamp(0.0, 20.0);
    (90.0 / 2f64.powf(z)).max(0.00002)
}
