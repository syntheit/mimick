//! Lightbox image viewer: full-screen preview with zoom, pan, EXIF details, and keyboard navigation.
//!
//! Loads preview or original resolution images with pinch-zoom and
//! swipe navigation. Displays an EXIF metadata panel and provides
//! download-to-folder and delete-to-trash actions.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use glib::clone;
use gstreamer as gst;
use gstreamer_play as gstplay;
use gtk::prelude::*;
use libadwaita::prelude::*;
use libadwaita::{CallbackAnimationTarget, TimedAnimation};

use crate::api_client::{ExifInfo, ThumbnailSize};
use crate::library::asset_object::AssetObject;
use crate::library::local_exif::{self, LocalExif};

use super::context_menu::show_asset_context_menu;
use super::download::{
    begin_download_session, finish_download_item, open_local_with_default_app, start_download,
    track_download_item,
};
use super::{LOCAL_ID_PREFIX, LibraryWindowUi, load_source_page, load_texture_oriented};

/// A single-child bin that applies a live vertical translation + fade to its
/// child purely at paint time (GPU transform, no relayout). Used to drive the
/// swipe-down-to-dismiss motion of the lightbox image without forcing a
/// `ScrolledWindow` relayout every frame (which `margin_top` would).
mod dismiss_bin {
    use std::cell::Cell;

    use gtk::glib;
    use gtk::graphene;
    use gtk::gsk;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;

    mod imp {
        use super::*;

        #[derive(Default)]
        pub struct DismissBin {
            pub offset_x: Cell<f64>,
            pub offset_y: Cell<f64>,
            pub opacity: Cell<f64>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for DismissBin {
            const NAME: &'static str = "MimickDismissBin";
            type Type = super::DismissBin;
            type ParentType = gtk::Widget;
        }

        impl ObjectImpl for DismissBin {
            fn constructed(&self) {
                self.parent_constructed();
                self.opacity.set(1.0);
                // Bin layout: measure/allocate the single child to fill us, so
                // we never have to override measure()/size_allocate().
                self.obj()
                    .set_layout_manager(Some(gtk::BinLayout::new()));
            }

            fn dispose(&self) {
                while let Some(child) = self.obj().first_child() {
                    child.unparent();
                }
            }
        }

        impl WidgetImpl for DismissBin {
            fn snapshot(&self, snapshot: &gtk::Snapshot) {
                let widget = self.obj();
                let opacity = self.opacity.get().clamp(0.0, 1.0);
                let offset_x = self.offset_x.get();
                let offset_y = self.offset_y.get();
                let faded = opacity < 0.999;
                if faded {
                    snapshot.push_opacity(opacity);
                }
                if offset_x.abs() > 0.01 || offset_y.abs() > 0.01 {
                    let transform = gsk::Transform::new().translate(&graphene::Point::new(
                        offset_x as f32,
                        offset_y as f32,
                    ));
                    snapshot.transform(Some(&transform));
                }
                // Paint the (single) child at the transformed origin.
                if let Some(child) = widget.first_child() {
                    widget.snapshot_child(&child, snapshot);
                }
                if faded {
                    snapshot.pop();
                }
            }
        }
    }

    glib::wrapper! {
        pub struct DismissBin(ObjectSubclass<imp::DismissBin>)
            @extends gtk::Widget,
            @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
    }

    impl DismissBin {
        pub fn new() -> Self {
            glib::Object::builder().build()
        }

        /// Set the single child widget (unparenting any previous one).
        pub fn set_child(&self, child: &impl IsA<gtk::Widget>) {
            while let Some(existing) = self.first_child() {
                existing.unparent();
            }
            child.unparent();
            child.set_parent(self);
        }

        /// Store the current dismiss translation (downward, px) and update the
        /// derived opacity, then invalidate paint only (no relayout).
        ///
        /// `viewport_h` is the height used to normalise the fade; the image
        /// fades toward 0.4 opacity as it is dragged a viewport-height down,
        /// never fully vanishing while still on-screen so the drag reads as
        /// interactive.
        pub fn set_dismiss_offset(&self, offset_y: f64, viewport_h: f64) {
            let clamped = offset_y.max(0.0);
            let imp = self.imp();
            imp.offset_x.set(0.0);
            imp.offset_y.set(clamped);
            let h = viewport_h.max(1.0);
            let fade = (clamped / h).min(0.6);
            imp.opacity.set(1.0 - fade);
            self.queue_draw();
        }

        /// Store a live *horizontal* drag translation (px; +right / -left) used
        /// for finger-following prev/next navigation. Keeps the image fully
        /// opaque (no fade — that's reserved for the downward dismiss) and
        /// clears any vertical offset. Paint-only invalidation, no relayout.
        pub fn set_nav_offset(&self, offset_x: f64) {
            let imp = self.imp();
            imp.offset_x.set(offset_x);
            imp.offset_y.set(0.0);
            imp.opacity.set(1.0);
            self.queue_draw();
        }

        /// Reset translation + opacity (freshly rendered / dismissed photo).
        pub fn reset_dismiss(&self) {
            let imp = self.imp();
            imp.offset_x.set(0.0);
            imp.offset_y.set(0.0);
            imp.opacity.set(1.0);
            self.queue_draw();
        }
    }

    impl Default for DismissBin {
        fn default() -> Self {
            Self::new()
        }
    }
}

use dismiss_bin::DismissBin;

/// Format a `gst::ClockTime` (or a raw seconds count) as `mm:ss` (or `h:mm:ss`
/// past an hour) for the video scrubber's time labels.
fn format_time(seconds: u64) -> String {
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The Immich-mobile-style playback controls that ride in the bottom chrome for
/// videos: a play/pause toggle, a scrub bar, and current/duration time labels.
/// Shared by value (all fields are `Rc`/GObject clones) so the `LightboxVideo`
/// bus watch can update them and the lightbox can reset them across navigation.
#[derive(Clone)]
struct VideoControls {
    play_pause: gtk::Button,
    scale: gtk::Scale,
    position_label: gtk::Label,
    duration_label: gtk::Label,
    /// Set while the user is dragging the scrubber, so incoming
    /// `PositionUpdated` messages don't yank the thumb out from under them.
    seeking: Rc<Cell<bool>>,
    /// Last known duration (nanoseconds); the scale runs 0..duration_ns.
    duration_ns: Rc<Cell<u64>>,
}

/// Inline video playback backed by a GStreamer `gtk4paintablesink` pipeline
/// (the approach Delfin uses), replacing `gtk::Video`/`gtk::MediaFile` which
/// fail silently for Immich's transcoded streams.
///
/// The widget handed to the caller (`picture`) renders the sink's
/// `gdk::Paintable`; the `Play` drives decode/seek/playback. Construction is
/// fallible: if the `gtk4paintablesink` element can't be created (plugin not
/// installed) we return `None` and the lightbox stays on the still poster.
struct LightboxVideo {
    play: gstplay::Play,
    picture: gtk::Picture,
    controls: VideoControls,
    /// Keeps the message-bus watch alive: the `BusWatchGuard` removes the watch
    /// on drop, so it must outlive the pipeline.
    _bus_watch: gst::bus::BusWatchGuard,
}

impl LightboxVideo {
    /// Build the pipeline and wire the playback `controls` to it. `on_error`
    /// fires on the main thread when the play pipeline posts a fatal error, so
    /// the caller can fall the media stack back to the still poster. Returns
    /// `None` (with a log line) if GStreamer or the `gtk4paintablesink` plugin
    /// is unavailable, so callers degrade gracefully rather than showing a black
    /// frame.
    fn new(controls: VideoControls, on_error: impl Fn() + 'static) -> Option<Self> {
        let sink = match gst::ElementFactory::make("gtk4paintablesink").build() {
            Ok(sink) => sink,
            Err(err) => {
                log::warn!(
                    "gtk4paintablesink unavailable ({err}); inline video disabled, using poster"
                );
                return None;
            }
        };
        // The paintable is a plain gdk::Paintable property on the sink; because
        // the whole tree resolves to a single glib 0.22, this is the *same*
        // GObject type gtk4 expects — no cross-version paintable mismatch.
        let paintable: gtk::gdk::Paintable = sink.property("paintable");

        // Wrap the sink in a glsinkbin so GStreamer can negotiate GL/DMABuf
        // upload paths; fall back to the bare sink if glsinkbin is missing.
        let video_sink = gst::ElementFactory::make("glsinkbin")
            .property("sink", &sink)
            .build()
            .unwrap_or_else(|_| sink.clone());

        let renderer = gstplay::PlayVideoOverlayVideoRenderer::with_sink(&video_sink);
        let play = gstplay::Play::new(Some(renderer));

        // Post position updates ~4×/second so the scrubber tracks playback
        // smoothly without flooding the main loop.
        let mut config = play.config();
        config.set_position_update_interval(250);
        let _ = play.set_config(config);

        let picture = gtk::Picture::builder()
            .paintable(&paintable)
            .content_fit(gtk::ContentFit::Contain)
            .vexpand(true)
            .hexpand(true)
            .build();

        // ── Wire the controls to this pipeline ──────────────────────────
        // Play/pause toggle: flip the pipeline state based on the icon the last
        // StateChanged message left showing (pause icon ⇒ currently playing).
        // The icon is then corrected authoritatively by StateChanged below.
        {
            let play = play.clone();
            let btn = controls.play_pause.clone();
            controls.play_pause.connect_clicked(move |_| {
                let is_playing =
                    btn.icon_name().as_deref() == Some("media-playback-pause-symbolic");
                if is_playing {
                    play.pause();
                } else {
                    play.play();
                }
            });
        }
        // Scrubber → seek. `connect_change_value` fires for both drags and
        // keyboard/step changes; we map the slider value (nanoseconds) to a
        // `ClockTime` and seek. The `seeking` guard (set on drag begin, cleared
        // on drag end) keeps `PositionUpdated` from fighting the thumb.
        {
            // `connect_change_value` fires for BOTH user drags and our own
            // programmatic `set_value` (from PositionUpdated). Only seek while the
            // user is actively dragging (`seeking` set by the press gesture);
            // otherwise this is a playback-driven update and seeking would fight
            // the pipeline. The bus watch already skips `set_value` while seeking,
            // so the two never form a feedback loop.
            let play_change = play.clone();
            let seeking_change = controls.seeking.clone();
            controls
                .scale
                .connect_change_value(move |_scale, _scroll, value| {
                    if seeking_change.get() {
                        let ns = value.max(0.0) as u64;
                        play_change.seek(gst::ClockTime::from_nseconds(ns));
                    }
                    glib::Propagation::Proceed
                });
            // Track drag begin/end via a click gesture so position updates stand
            // down only for the duration of the drag.
            let press = gtk::GestureClick::new();
            let seeking_press = controls.seeking.clone();
            press.connect_pressed(move |_, _, _, _| seeking_press.set(true));
            let seeking_release = controls.seeking.clone();
            let play_release = play.clone();
            let scale_release = controls.scale.clone();
            press.connect_released(move |_, _, _, _| {
                // Final seek to the released position, then release the guard.
                let ns = scale_release.value().max(0.0) as u64;
                play_release.seek(gst::ClockTime::from_nseconds(ns));
                seeking_release.set(false);
            });
            controls.scale.add_controller(press);
        }

        // Watch the play message bus for fatal errors (fall back to the poster,
        // not a silent black frame) and for the playback state we surface in the
        // controls (position → scrubber/label, duration → range/label, state →
        // play/pause icon). The `Play` posts its own application messages, so we
        // detect them via `PlayMessage::parse` rather than the raw `MessageView`.
        // The bus starts flushing; enable delivery, then attach a local
        // (main-thread) watch and keep its guard alive in the struct.
        let bus = play.message_bus();
        bus.set_flushing(false);
        let controls_watch = controls.clone();
        let bus_watch = bus
            .add_watch_local(move |_, msg| {
                if !gstplay::Play::is_play_message(msg) {
                    return glib::ControlFlow::Continue;
                }
                match gstplay::PlayMessage::parse(msg) {
                    Ok(gstplay::PlayMessage::Error(err)) => {
                        log::warn!("Inline video playback error: {}", err.error());
                        on_error();
                    }
                    Ok(gstplay::PlayMessage::DurationChanged(dc)) => {
                        let ns = dc.duration().map(|d| d.nseconds()).unwrap_or(0);
                        controls_watch.duration_ns.set(ns);
                        controls_watch.scale.set_range(0.0, ns.max(1) as f64);
                        controls_watch
                            .duration_label
                            .set_label(&format_time(ns / 1_000_000_000));
                    }
                    Ok(gstplay::PlayMessage::PositionUpdated(pu)) => {
                        if !controls_watch.seeking.get() {
                            let ns = pu.position().map(|p| p.nseconds()).unwrap_or(0);
                            controls_watch.scale.set_value(ns as f64);
                            controls_watch
                                .position_label
                                .set_label(&format_time(ns / 1_000_000_000));
                        }
                    }
                    Ok(gstplay::PlayMessage::StateChanged(sc)) => {
                        // Reflect the real pipeline state on the toggle icon.
                        let playing = sc.state() == gstplay::PlayState::Playing;
                        controls_watch.play_pause.set_icon_name(if playing {
                            "media-playback-pause-symbolic"
                        } else {
                            "media-playback-start-symbolic"
                        });
                    }
                    _ => {}
                }
                glib::ControlFlow::Continue
            })
            .ok()?;

        Some(Self {
            play,
            picture,
            controls,
            _bus_watch: bus_watch,
        })
    }

    /// Start (or restart) playback of `uri`.
    fn play_uri(&self, uri: &str) {
        self.play.set_uri(Some(uri));
        self.play.play();
    }

    /// Stop playback and clear the URI so the previous stream fully releases
    /// its decoder/network resources when navigating away or dismissing.
    fn stop(&self) {
        self.play.stop();
        self.play.set_uri(None);
        self.controls.seeking.set(false);
    }
}

/// Everything we can learn about a local file off the main thread before
/// handing the data back to GTK. `exif` is the cached EXIF parse; `dims`
/// comes from a pixbuf header-only read; `mtime_iso` falls back when the
/// file has no `DateTimeOriginal` so we always show *some* date.
struct LocalProbe {
    exif: Option<LocalExif>,
    file_size: Option<u64>,
    mtime_iso: Option<String>,
    dims: Option<(u32, u32)>,
}

/// Render a `SystemTime` as an RFC3339 string in the local timezone so the
/// existing `format_datetime_display` pipeline can format it consistently
/// with EXIF-derived timestamps.
fn systemtime_to_rfc3339(t: std::time::SystemTime) -> String {
    use chrono::{DateTime, Local};
    let dt: DateTime<Local> = t.into();
    dt.to_rfc3339()
}

/// Project local-file metadata into the API EXIF shape so the existing
/// renderer handles both sources uniformly.
fn exif_from_local(local: &LocalExif, file_size: Option<u64>) -> ExifInfo {
    ExifInfo {
        make: local.make.clone(),
        model: local.model.clone(),
        lens_model: local.lens_model.clone(),
        f_number: local.f_number,
        focal_length: local.focal_length,
        iso: local.iso,
        exposure_time: local.exposure_time.clone(),
        file_size_in_byte: file_size,
        date_time_original: local.date_time_original.clone(),
        city: None,
        state: None,
        country: None,
        latitude: local.latitude,
        longitude: local.longitude,
        description: local.description.clone(),
        exif_image_width: local.image_width,
        exif_image_height: local.image_height,
    }
}

fn original_preview_cache_path(
    cache_dir: &std::path::Path,
    asset_id: &str,
    filename: &str,
) -> std::path::PathBuf {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .unwrap_or("bin");
    cache_dir.join(format!("{asset_id}.{ext}"))
}

/// Create a properly-named export copy of a cached file for drag-out.
///
/// Returns `Some(path)` with the original filename visible to file managers.
/// Falls back to `source` if the export directory isn't available.
fn drag_export_path(
    asset_id: &str,
    filename: &str,
    source: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let export_dir = crate::profile::cache_dir()?.join("drag_export");
    let _ = std::fs::create_dir_all(&export_dir);
    let prefix = &asset_id[..8.min(asset_id.len())];
    let export = export_dir.join(format!("{prefix}_{filename}"));
    if export.exists() {
        return Some(export);
    }
    if std::fs::hard_link(source, &export).is_ok() || std::fs::copy(source, &export).is_ok() {
        Some(export)
    } else {
        Some(source.to_path_buf())
    }
}

/// Populate the details sidebar with sectioned EXIF metadata.
///
/// Groups fall into three categories — Camera, Image, Location — each rendered
/// as an `AdwPreferencesGroup` with accent-coloured prefix icons. Empty groups
/// (e.g. an image with no GPS) are skipped so the pane stays compact.
///
/// `taken_label` decides whether the date row reads "Taken" (EXIF
/// DateTimeOriginal — the camera's capture moment) or "Modified" (filesystem
/// mtime fallback when no capture timestamp exists).
fn fill_exif_box(container: &gtk::Box, exif: &crate::api_client::ExifInfo, taken_label: &str) {
    if let Some(group) = build_camera_group(exif) {
        container.append(&group);
    }
    if let Some(group) = build_image_group(exif, taken_label) {
        container.append(&group);
    }
    if let Some(group) = build_location_group(exif) {
        container.append(&group);
    }
    if let Some(desc) = &exif.description
        && !desc.trim().is_empty()
    {
        let note_group = libadwaita::PreferencesGroup::builder()
            .title("Description")
            .build();
        let row = libadwaita::ActionRow::builder()
            .title(desc.as_str())
            .title_lines(0)
            .css_classes(["property"])
            .build();
        note_group.add(&row);
        container.append(&note_group);
    }
}

fn build_camera_group(exif: &crate::api_client::ExifInfo) -> Option<libadwaita::PreferencesGroup> {
    let group = libadwaita::PreferencesGroup::builder()
        .title("Camera")
        .build();
    let mut rows = 0u32;
    if let Some(c) = format_camera(exif) {
        group.add(&accent_row(
            "camera-photo-symbolic",
            "mimick-accent-camera",
            "Body",
            &c,
        ));
        rows += 1;
    }
    if let Some(l) = &exif.lens_model
        && !l.trim().is_empty()
    {
        group.add(&accent_row(
            "view-fullscreen-symbolic",
            "mimick-accent-camera",
            "Lens",
            l,
        ));
        rows += 1;
    }
    if let Some(exposure) = format_exposure(exif) {
        group.add(&accent_row(
            "weather-clear-symbolic",
            "mimick-accent-camera",
            "Exposure",
            &exposure,
        ));
        rows += 1;
    }
    (rows > 0).then_some(group)
}

fn build_image_group(
    exif: &crate::api_client::ExifInfo,
    taken_label: &str,
) -> Option<libadwaita::PreferencesGroup> {
    let group = libadwaita::PreferencesGroup::builder()
        .title("Image")
        .build();
    let mut rows = 0u32;
    if let (Some(w), Some(h)) = (exif.exif_image_width, exif.exif_image_height) {
        group.add(&accent_row(
            "view-grid-symbolic",
            "mimick-accent-image",
            "Dimensions",
            &format!("{w} × {h}"),
        ));
        rows += 1;
    }
    if let Some(size) = exif.file_size_in_byte {
        group.add(&accent_row(
            "drive-harddisk-symbolic",
            "mimick-accent-image",
            "Size",
            &format_bytes(size),
        ));
        rows += 1;
    }
    if let Some(dt) = &exif.date_time_original
        && !dt.trim().is_empty()
    {
        group.add(&accent_row(
            "x-office-calendar-symbolic",
            "mimick-accent-image",
            taken_label,
            &format_datetime_display(dt),
        ));
        rows += 1;
    }
    (rows > 0).then_some(group)
}

fn build_location_group(
    exif: &crate::api_client::ExifInfo,
) -> Option<libadwaita::PreferencesGroup> {
    let group = libadwaita::PreferencesGroup::builder()
        .title("Location")
        .build();
    let mut rows = 0u32;
    if let Some(loc) = format_location(exif) {
        group.add(&accent_row(
            "mark-location-symbolic",
            "mimick-accent-location",
            "Place",
            &loc,
        ));
        rows += 1;
    }
    if let (Some(lat), Some(lon)) = (exif.latitude, exif.longitude) {
        group.add(&accent_row(
            "find-location-symbolic",
            "mimick-accent-location",
            "Coordinates",
            &format!("{lat:.5}, {lon:.5}"),
        ));
        rows += 1;
    }
    (rows > 0).then_some(group)
}

fn accent_row(icon: &str, accent_class: &str, title: &str, value: &str) -> libadwaita::ActionRow {
    let prefix = gtk::Image::builder()
        .icon_name(icon)
        .pixel_size(16)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .css_classes(["mimick-detail-icon", accent_class])
        .build();
    let row = libadwaita::ActionRow::builder()
        .title(title)
        .subtitle(value)
        .subtitle_lines(2)
        .css_classes(["property"])
        .build();
    row.add_prefix(&prefix);
    row
}

fn format_camera(exif: &crate::api_client::ExifInfo) -> Option<String> {
    match (&exif.make, &exif.model) {
        (Some(m), Some(n)) => {
            let m = m.trim();
            let n = n.trim();
            if n.starts_with(m) {
                Some(n.to_string())
            } else {
                Some(format!("{m} {n}"))
            }
        }
        (Some(m), None) => Some(m.trim().to_string()),
        (None, Some(n)) => Some(n.trim().to_string()),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

fn format_exposure(exif: &crate::api_client::ExifInfo) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(f) = exif.f_number {
        parts.push(format!("ƒ/{f:.1}"));
    }
    if let Some(et) = &exif.exposure_time
        && !et.trim().is_empty()
    {
        parts.push(et.trim().to_string());
    }
    if let Some(iso) = exif.iso {
        parts.push(format!("ISO {iso}"));
    }
    if let Some(focal) = exif.focal_length {
        parts.push(format!("{focal:.0}mm"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn format_location(exif: &crate::api_client::ExifInfo) -> Option<String> {
    let parts: Vec<&str> = [&exif.city, &exif.state, &exif.country]
        .into_iter()
        .filter_map(|s| s.as_deref().map(str::trim).filter(|t| !t.is_empty()))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// Format a byte count value into a human-readable size string (e.g. KB, MB, GB).
fn format_bytes(n: u64) -> String {
    const KIB: f64 = 1024.0;
    let n_f = n as f64;
    if n_f >= KIB * KIB * KIB {
        format!("{:.2} GB", n_f / (KIB * KIB * KIB))
    } else if n_f >= KIB * KIB {
        format!("{:.2} MB", n_f / (KIB * KIB))
    } else if n_f >= KIB {
        format!("{:.1} KB", n_f / KIB)
    } else {
        format!("{} B", n)
    }
}

/// Format an ISO 8601 timestamp for display, converting from UTC to the
/// user's local timezone.
///
/// Immich normalises `date_time_original` and `fileCreatedAt` to UTC before
/// storage, so a photo taken at 19:55:15+05:30 is stored as
/// 2024-01-15T14:25:15.000Z. We parse the UTC value and convert it to the
/// system's local timezone so the displayed time matches what the camera
/// originally recorded. Falls back to the raw string if parsing fails.
fn format_datetime_display(iso: &str) -> String {
    use chrono::{DateTime, Local, Utc};
    // Try offset-aware parse first (handles +05:30, Z, etc.)
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        let local: DateTime<Local> = dt.into();
        return local.format("%Y-%m-%d %H:%M:%S UTC%:z").to_string();
    }
    // Fallback: try treating as UTC
    if let Ok(dt) = iso.parse::<DateTime<Utc>>() {
        let local: DateTime<Local> = dt.into();
        return local.format("%Y-%m-%d %H:%M:%S UTC%:z").to_string();
    }
    // Last resort: strip trailing fractional seconds / timezone suffix
    iso.get(..19).unwrap_or(iso).replace('T', " ").to_string()
}

/// Format an ISO 8601 timestamp as a long, human-friendly local string, e.g.
/// "Monday, July 19, 2026 · 3:42 PM". Falls back to the compact display form
/// (and finally the raw string) if the value can't be parsed.
fn format_full_timestamp(iso: &str) -> String {
    use chrono::{DateTime, Local, Utc};
    let local: Option<DateTime<Local>> = DateTime::parse_from_rfc3339(iso)
        .map(|dt| dt.into())
        .ok()
        .or_else(|| iso.parse::<DateTime<Utc>>().ok().map(|dt| dt.into()));
    if let Some(local) = local {
        // %-I / %-M strip leading zeros on Unix. Weekday, month name, day, year.
        return local.format("%A, %B %-d, %Y · %-I:%M %p").to_string();
    }
    let compact = format_datetime_display(iso);
    if compact.trim().is_empty() {
        iso.to_string()
    } else {
        compact
    }
}

/// Short date/time for the lightbox top bar, e.g. "Jul 19, 2026, 3:42 PM".
/// Empty string when the timestamp can't be parsed so the caller can hide it.
fn format_topbar_datetime(iso: &str) -> String {
    use chrono::{DateTime, Local, Utc};
    let local: Option<DateTime<Local>> = DateTime::parse_from_rfc3339(iso)
        .map(|dt| dt.into())
        .ok()
        .or_else(|| iso.parse::<DateTime<Utc>>().ok().map(|dt| dt.into()));
    local
        .map(|l| l.format("%b %-d, %Y · %-I:%M %p").to_string())
        .unwrap_or_default()
}

/// Build a small OpenStreetMap minimap centered on `(lat, lon)` with a marker,
/// ~180px tall at zoom 14. Uses libshumate's `SimpleMap` with an OSM raster
/// tile source and a single marker layer.
fn build_minimap(lat: f64, lon: f64) -> Option<gtk::Widget> {
    use libshumate::prelude::*;

    let map = libshumate::SimpleMap::new();
    map.set_height_request(180);
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

    let viewport = map.viewport()?;
    viewport.set_zoom_level(14.0);
    viewport.set_location(lat, lon);

    // Drop a marker at the coordinates.
    let marker = libshumate::Marker::new();
    marker.set_location(lat, lon);
    let pin = gtk::Image::from_icon_name("mark-location-symbolic");
    pin.set_pixel_size(28);
    pin.add_css_class("error");
    marker.set_child(Some(&pin));

    let marker_layer = libshumate::MarkerLayer::new_full(&viewport, gtk::SelectionMode::None);
    marker_layer.add_marker(&marker);
    map.add_overlay_layer(&marker_layer);

    Some(map.upcast())
}

/// Apply zoom to a lightbox Picture. Zoom is fit-relative: 1.0 = the size the
/// texture would occupy inside `viewer` under Contain layout. >1.0 overflows
/// the viewer for panning. Returns the computed content dimensions when zoomed.
fn apply_lightbox_zoom(
    picture: &gtk::Picture,
    viewer: &gtk::ScrolledWindow,
    zoom: f64,
) -> Option<(f64, f64)> {
    if (zoom - 1.0).abs() < 0.001 {
        picture.set_size_request(-1, -1);
        return None;
    }
    let Some(paintable) = picture.paintable() else {
        picture.set_size_request(-1, -1);
        return None;
    };
    let nw = paintable.intrinsic_width().max(1) as f64;
    let nh = paintable.intrinsic_height().max(1) as f64;
    let viewer_w = viewer.width().max(1) as f64;
    let viewer_h = viewer.height().max(1) as f64;
    let texture_aspect = nw / nh;
    let viewer_aspect = viewer_w / viewer_h;
    let (fit_w, fit_h) = if viewer_aspect > texture_aspect {
        (viewer_h * texture_aspect, viewer_h)
    } else {
        (viewer_w, viewer_w / texture_aspect)
    };
    let cw = fit_w * zoom;
    let ch = fit_h * zoom;
    picture.set_size_request(cw as i32, ch as i32);
    Some((cw, ch))
}

/// Present a simple modal alert anchored to the main window.
fn show_lightbox_alert(ui: &LibraryWindowUi, heading: &str, body: &str) {
    let alert = libadwaita::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .build();
    alert.add_response("ok", "OK");
    alert.present(Some(&ui.window));
}

/// Show an album-picker dialog and add `asset_id` to the chosen album.
///
/// Fetches the album list, then presents an `AdwAlertDialog` whose body is a
/// scrollable list of album buttons. Picking one issues the add-to-album call.
fn show_add_to_album_dialog(ui: Rc<LibraryWindowUi>, asset_id: String) {
    glib::MainContext::default().spawn_local(async move {
        let albums = match ui.ctx.api_client.get_all_albums().await {
            Ok(a) => a,
            Err(err) => {
                show_lightbox_alert(&ui, "Add to album", &err);
                return;
            }
        };
        if albums.is_empty() {
            show_lightbox_alert(&ui, "Add to album", "No albums found on the server.");
            return;
        }
        let mut albums = albums;
        albums.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

        let dialog = libadwaita::AlertDialog::builder()
            .heading("Add to album")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.set_close_response("cancel");

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        for (name, album_id) in &albums {
            let row = libadwaita::ActionRow::builder()
                .title(name)
                .activatable(true)
                .build();
            row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
            let ui = ui.clone();
            let asset_id = asset_id.clone();
            let album_id = album_id.clone();
            let name = name.clone();
            let dialog_weak = dialog.downgrade();
            row.connect_activated(move |_| {
                if let Some(dialog) = dialog_weak.upgrade() {
                    dialog.close();
                }
                let ui = ui.clone();
                let asset_id = asset_id.clone();
                let album_id = album_id.clone();
                let name = name.clone();
                glib::MainContext::default().spawn_local(async move {
                    let ok = ui
                        .ctx
                        .api_client
                        .add_assets_to_album(&album_id, &[asset_id])
                        .await;
                    if !ok {
                        show_lightbox_alert(
                            &ui,
                            "Add to album",
                            &format!("Failed to add to \"{name}\"."),
                        );
                    }
                });
            });
            list.append(&row);
        }
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(240)
            .propagate_natural_height(true)
            .child(&list)
            .build();
        dialog.set_extra_child(Some(&scroller));
        dialog.present(Some(&ui.window));
    });
}

/// Construct and present the Immich-mobile-style fullscreen lightbox.
///
/// The viewer is a black canvas with no chrome by default. A single tap on the
/// image toggles two translucent bars (top: back / date / favorite / menu;
/// bottom: share / add-to-album / delete). Horizontal swipes navigate between
/// photos, a downward swipe dismisses the viewer, and an upward swipe opens the
/// details bottom sheet. Pinch-zoom / pan and inline video playback are kept.
pub(super) fn open_lightbox(ui: Rc<LibraryWindowUi>, position: u32) {
    let Some(_item) = ui.grid.model.item(position).and_downcast::<AssetObject>() else {
        return;
    };

    let page = libadwaita::NavigationPage::builder()
        .title("Photo")
        // Disable the AdwNavigationView edge-swipe-back and auto back button so
        // a horizontal drag navigates prev/next instead of popping the page.
        // The chrome has its own back button; programmatic `nav.pop()` still
        // works regardless of `can-pop` (it only gates the gesture/auto-button).
        .can_pop(false)
        .build();

    // Root bottom sheet: content is the black viewer overlay; the sheet is the
    // swipe-up details drawer.
    let sheet = libadwaita::BottomSheet::builder()
        .can_open(true)
        .can_close(true)
        .modal(true)
        .build();

    let viewer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .css_classes(["mimick-viewer-black"])
        .build();

    // Two picture widgets in a stack so navigation can slide between them.
    let picture_a = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .vexpand(true)
        .hexpand(true)
        .css_classes(["mimick-lightbox-picture"])
        .build();
    let picture_b = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .vexpand(true)
        .hexpand(true)
        .css_classes(["mimick-lightbox-picture"])
        .build();
    let pic_stack = gtk::Stack::builder()
        .transition_duration(180)
        .vexpand(true)
        .hexpand(true)
        .build();
    pic_stack.add_named(&picture_a, Some("a"));
    pic_stack.add_named(&picture_b, Some("b"));
    pic_stack.set_visible_child_name("a");
    // Wrap the picture stack in a bin that applies the swipe-down-to-dismiss
    // translation + fade as a GPU transform at paint time (no relayout).
    let dismiss_bin = DismissBin::new();
    dismiss_bin.set_hexpand(true);
    dismiss_bin.set_vexpand(true);
    dismiss_bin.set_child(&pic_stack);
    let scrolled_picture = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::External)
        .child(&dismiss_bin)
        .vexpand(true)
        .hexpand(true)
        .kinetic_scrolling(false)
        .build();

    // Spinner overlay: a centered Mimick app icon that rotates while a
    // full-resolution texture is being fetched / decoded.
    let loader_icon = gtk::Image::builder()
        .icon_name("dev.nicx.mimick")
        .pixel_size(72)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["mimick-loader-icon"])
        .build();
    let loader_overlay = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::Crossfade)
        .transition_duration(180)
        .reveal_child(false)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .child(&loader_icon)
        .can_target(false)
        .build();
    let picture_overlay = gtk::Overlay::builder().build();
    picture_overlay.set_child(Some(&scrolled_picture));
    picture_overlay.add_overlay(&loader_overlay);

    let unavailable_title = gtk::Label::builder()
        .label("Preview unavailable")
        .css_classes(["title-3"])
        .build();
    let unavailable_filename = gtk::Label::builder()
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .max_width_chars(42)
        .build();
    let unavailable_mime = gtk::Label::builder().css_classes(["dim-label"]).build();
    let unavailable_open = gtk::Button::builder()
        .label("Open in external app")
        .css_classes(["suggested-action"])
        .build();
    let unavailable_path = Rc::new(RefCell::new(None::<String>));
    unavailable_open.connect_clicked({
        let unavailable_path = unavailable_path.clone();
        move |_| {
            if let Some(path) = unavailable_path.borrow().as_deref() {
                open_local_with_default_app(path);
            }
        }
    });
    let unavailable_card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["mimick-preview-unavailable"])
        .build();
    unavailable_card.append(&unavailable_title);
    unavailable_card.append(&unavailable_filename);
    unavailable_card.append(&unavailable_mime);
    unavailable_card.append(&unavailable_open);
    let unavailable_overlay = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::Crossfade)
        .transition_duration(180)
        .reveal_child(false)
        .halign(gtk::Align::Fill)
        .valign(gtk::Align::Fill)
        .child(&unavailable_card)
        .can_target(false)
        .build();
    picture_overlay.add_overlay(&unavailable_overlay);

    // Drag source for exporting the current asset's original file.
    let lightbox_drag_path: Rc<RefCell<Option<std::path::PathBuf>>> = Rc::new(RefCell::new(None));
    {
        let drag_source = gtk::DragSource::new();
        drag_source.set_actions(gtk::gdk::DragAction::COPY);
        let drag_path = lightbox_drag_path.clone();
        drag_source.connect_prepare(move |_source, _x, _y| {
            let path = drag_path.borrow().clone()?;
            if !path.exists() {
                return None;
            }
            let file = gtk::gio::File::for_path(&path);
            Some(gtk::gdk::ContentProvider::for_value(&file.to_value()))
        });
        picture_overlay.add_controller(drag_source);
    }

    let active_a = Rc::new(Cell::new(true));
    let zoom_level = Rc::new(Cell::new(1.0_f64));
    // Set for the lifetime of a single-finger directional nav/dismiss drag
    // (see `nav_drag`). While set, the zoom/pan gestures on `scrolled_picture`
    // (`double_click`, `pan_drag`) stand down so a swipe can never be
    // reinterpreted as a zoom or a pan. Cleared on that drag's end.
    let swipe_active = Rc::new(Cell::new(false));
    let resolution_full = ui.ctx.config.read().data.library_preview_full_resolution;

    let media_stack = gtk::Stack::builder()
        .vexpand(true)
        .hexpand(true)
        .transition_type(gtk::StackTransitionType::None)
        .build();
    media_stack.add_named(&picture_overlay, Some("image"));
    media_stack.set_visible_child_name("image");

    // Transparent full-area input layer for VIDEO ONLY (overlaid on
    // `chrome_overlay` below, above `viewer`/`media_stack`).
    //
    // While a `gtk4paintablesink` video is actively PLAYING, GTK promotes the
    // sink's paintable to a GL/dmabuf (graphics-offload) subsurface. That
    // subsurface owns pointer input at the compositor level and swallows events
    // before they reach the tap/`nav_drag` controllers on `media_stack` (an
    // *ancestor* of the video picture) — which is why gestures were dead over a
    // playing video but fine when paused (no active subsurface). Events still
    // work over a still image because there is no such subsurface.
    //
    // Fix: a sibling layer that sits ABOVE `viewer`/`media_stack` in the overlay
    // stack. In a GtkOverlay only the *topmost pickable* widget under the
    // pointer becomes the event target, so this layer — when pickable — reliably
    // receives tap + swipe regardless of the video's rendering path. It carries
    // its own tap + `nav_drag` controllers (wired further down) that drive the
    // SAME chrome/navigation helpers as the image-side ones.
    //
    // It must NOT steal input from images (their pinch/pan/double-tap live on
    // `scrolled_picture`, and only the topmost pickable widget is the target).
    // So the layer is `can_target(false)` by default and only flipped to `true`
    // while the video child is visible (`set_video_input_active`). Video has no
    // zoom/pan/double-tap, so nothing is lost when it intercepts.
    let video_input_layer = gtk::Box::builder()
        .hexpand(true)
        .vexpand(true)
        .can_target(false)
        .build();
    let set_video_input_active = {
        let video_input_layer = video_input_layer.clone();
        Rc::new(move |active: bool| {
            video_input_layer.set_can_target(active);
        })
    };

    // ── Video playback controls (Immich-mobile style) ───────────────────
    // Built here so the (lazily-constructed) `LightboxVideo` can bind them; they
    // are placed into the bottom chrome further down and revealed only when the
    // current asset is a video. Icon starts on "start" and is corrected by the
    // pipeline's StateChanged messages once playback begins.
    let video_play_pause = gtk::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text("Play/Pause")
        .css_classes(["flat", "circular"])
        .valign(gtk::Align::Center)
        .build();
    let video_position_label = gtk::Label::builder()
        .label("0:00")
        .css_classes(["mimick-lightbox-video-time"])
        .build();
    let video_duration_label = gtk::Label::builder()
        .label("0:00")
        .css_classes(["mimick-lightbox-video-time"])
        .build();
    let video_scale = gtk::Scale::builder()
        .orientation(gtk::Orientation::Horizontal)
        .hexpand(true)
        .draw_value(false)
        .valign(gtk::Align::Center)
        .css_classes(["mimick-lightbox-video-scale"])
        .build();
    video_scale.set_range(0.0, 1.0);
    let video_controls = VideoControls {
        play_pause: video_play_pause.clone(),
        scale: video_scale.clone(),
        position_label: video_position_label.clone(),
        duration_label: video_duration_label.clone(),
        seeking: Rc::new(Cell::new(false)),
        duration_ns: Rc::new(Cell::new(0)),
    };
    // Reset the controls to a fresh "starting playback" state (used whenever a
    // video (re)starts and whenever we navigate away from one).
    let video_reset_controls = {
        let controls = video_controls.clone();
        Rc::new(move || {
            controls.seeking.set(false);
            controls.duration_ns.set(0);
            controls.scale.set_range(0.0, 1.0);
            controls.scale.set_value(0.0);
            controls.position_label.set_label("0:00");
            controls.duration_label.set_label("0:00");
            controls
                .play_pause
                .set_icon_name("media-playback-start-symbolic");
        })
    };

    // Inline video playback via a GStreamer `gtk4paintablesink` pipeline (built
    // lazily, once, and reused across navigation). `None` when the plugin is
    // missing — in that case videos fall back to the still poster. On a fatal
    // pipeline error the watch flips the media stack back to the poster.
    let video: Rc<RefCell<Option<LightboxVideo>>> = Rc::new(RefCell::new(None));
    let ensure_video = {
        let video = video.clone();
        let media_stack = media_stack.clone();
        let set_video_input_active = set_video_input_active.clone();
        let video_controls = video_controls.clone();
        Rc::new(move || -> Option<gtk::Picture> {
            if video.borrow().is_none() {
                let media_stack_err = media_stack.clone();
                let set_video_input_active_err = set_video_input_active.clone();
                let Some(vp) = LightboxVideo::new(video_controls.clone(), move || {
                    // Fatal playback error: reveal the poster instead of a black
                    // frame so the failure is visible, not silent. Also release
                    // the video input layer so the still poster's gestures work.
                    media_stack_err.set_visible_child_name("image");
                    set_video_input_active_err(false);
                }) else {
                    return None;
                };
                media_stack.add_named(&vp.picture, Some("video"));
                *video.borrow_mut() = Some(vp);
            }
            video.borrow().as_ref().map(|vp| vp.picture.clone())
        })
    };

    // Stop playback whenever the lightbox page is hidden/popped. This covers
    // every exit path — back button, Escape, delete, and the swipe-down
    // dismiss — so a video never keeps decoding/streaming behind the grid.
    page.connect_hidden(clone!(
        #[strong]
        video,
        move |_| {
            if let Some(vp) = video.borrow().as_ref() {
                vp.stop();
            }
        }
    ));

    // ── Chrome: top bar and bottom bar (hidden by default) ──────────────
    let back_btn = gtk::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text("Back")
        .build();
    let datetime_label = gtk::Label::builder()
        .css_classes(["mimick-lightbox-datetime"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .hexpand(true)
        .halign(gtk::Align::Center)
        .build();
    let favorite_btn = gtk::ToggleButton::builder()
        .icon_name("non-starred-symbolic")
        .tooltip_text("Favorite")
        .build();
    let menu = gtk::gio::Menu::new();
    menu.append(Some("Download"), Some("lightbox.download"));
    menu.append(Some("Copy to clipboard"), Some("lightbox.copy"));
    menu.append(Some("Open with…"), Some("lightbox.openwith"));
    menu.append(Some("Full resolution"), Some("lightbox.fullres"));
    let menu_btn = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .menu_model(&menu)
        .tooltip_text("More")
        .build();
    let top_actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();
    top_actions.append(&favorite_btn);
    top_actions.append(&menu_btn);
    let topbar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .css_classes(["mimick-lightbox-topbar"])
        .build();
    topbar.append(&back_btn);
    topbar.append(&datetime_label);
    topbar.append(&top_actions);
    let top_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .transition_duration(160)
        .reveal_child(false)
        .valign(gtk::Align::Start)
        .child(&topbar)
        .build();

    let make_action = |icon: &str, label: &str| -> gtk::Button {
        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .halign(gtk::Align::Center)
            .build();
        let img = gtk::Image::from_icon_name(icon);
        img.set_pixel_size(20);
        let lbl = gtk::Label::new(Some(label));
        inner.append(&img);
        inner.append(&lbl);
        gtk::Button::builder()
            .child(&inner)
            .hexpand(true)
            .css_classes(["flat"])
            .build()
    };
    let share_btn = make_action("send-to-symbolic", "Share");
    let addalbum_btn = make_action("list-add-symbolic", "Album");
    let delete_btn = make_action("user-trash-symbolic", "Delete");
    let bottombar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .homogeneous(true)
        .css_classes(["mimick-lightbox-bottombar"])
        .build();
    bottombar.append(&share_btn);
    bottombar.append(&addalbum_btn);
    bottombar.append(&delete_btn);

    // Video playback controls row (play/pause · position · scrubber · duration),
    // stacked ABOVE the share/album/delete actions. Its own revealer toggles by
    // media kind so it shows only for videos, while the whole bottom chrome
    // appears/disappears with the tap via `bottom_revealer`.
    let video_controls_bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .css_classes(["mimick-lightbox-videobar"])
        .build();
    video_controls_bar.append(&video_play_pause);
    video_controls_bar.append(&video_position_label);
    video_controls_bar.append(&video_scale);
    video_controls_bar.append(&video_duration_label);
    let video_controls_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideUp)
        .transition_duration(140)
        .reveal_child(false)
        .child(&video_controls_bar)
        .build();

    let bottom_stack = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    bottom_stack.append(&video_controls_revealer);
    bottom_stack.append(&bottombar);
    let bottom_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideUp)
        .transition_duration(160)
        .reveal_child(false)
        .valign(gtk::Align::End)
        .child(&bottom_stack)
        .build();

    // Wrap the whole media stack (image OR video) in an outer DismissBin so a
    // swipe-down translates + fades whichever child is currently visible. The
    // inner `dismiss_bin` (around `pic_stack`, inside the ScrolledWindow) stays
    // in the tree purely for the image layout; it is kept at identity now — the
    // outer bin is the one the swipe gestures drive so the motion covers video
    // too. Attaching the tap/nav gestures at this level makes them fire
    // regardless of which media_stack child shows.
    let media_dismiss_bin = DismissBin::new();
    media_dismiss_bin.set_hexpand(true);
    media_dismiss_bin.set_vexpand(true);
    media_dismiss_bin.set_child(&media_stack);
    viewer.append(&media_dismiss_bin);
    // Overlay the chrome bars on top of the viewer.
    let chrome_overlay = gtk::Overlay::builder().build();
    chrome_overlay.set_child(Some(&viewer));
    // Add the transparent VIDEO input layer (created earlier) FIRST so the
    // chrome revealers, added next, stack ABOVE it and stay clickable
    // (back/favorite/⋮/share/album/delete + video controls). The layer only
    // needs to be above `viewer`.
    chrome_overlay.add_overlay(&video_input_layer);
    chrome_overlay.add_overlay(&top_revealer);
    chrome_overlay.add_overlay(&bottom_revealer);

    // ── Details drawer contents ─────────────────────────────────────────
    let details_inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(14)
        .margin_bottom(20)
        .margin_start(12)
        .margin_end(12)
        .build();
    let sheet_grabber = gtk::Box::builder()
        .halign(gtk::Align::Center)
        .width_request(36)
        .height_request(4)
        .css_classes(["mimick-sheet-grabber"])
        .build();
    let details_timestamp = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["title-4"])
        .build();
    // ── People / recognized faces row ────────────────────────────────
    // A "People" heading plus a horizontally-scrolling strip of circular
    // avatars (one per non-hidden recognized person). Built once here and
    // repopulated per-asset; hidden entirely when the current asset has no
    // people.
    let details_people = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .visible(false)
        .build();
    let details_people_title = gtk::Label::builder()
        .xalign(0.0)
        .label("People")
        .css_classes(["title-4"])
        .build();
    // Horizontal strip that holds the per-person avatar+name tiles.
    let details_people_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();
    let details_people_scroll = gtk::ScrolledWindow::builder()
        .child(&details_people_row)
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .build();
    details_people.append(&details_people_title);
    details_people.append(&details_people_scroll);
    let description_group = libadwaita::PreferencesGroup::builder()
        .title("Description")
        .build();
    let description_row = libadwaita::EntryRow::builder()
        .title("Add a description")
        .build();
    description_group.add(&description_row);
    let details_map_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    let details_exif = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    let details_loading = gtk::Label::builder()
        .xalign(0.0)
        .label("Loading details…")
        .css_classes(["dim-label"])
        .build();
    details_inner.append(&sheet_grabber);
    details_inner.append(&details_timestamp);
    details_inner.append(&details_people);
    details_inner.append(&description_group);
    details_inner.append(&details_map_box);
    details_inner.append(&details_loading);
    details_inner.append(&details_exif);
    let details_pane = gtk::ScrolledWindow::builder()
        .child(&details_inner)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(560)
        .build();

    sheet.set_content(Some(&chrome_overlay));
    sheet.set_sheet(Some(&details_pane));
    page.set_child(Some(&sheet));

    // Chrome visibility toggle state.
    let chrome_visible = Rc::new(Cell::new(false));
    let set_chrome = {
        let top_revealer = top_revealer.clone();
        let bottom_revealer = bottom_revealer.clone();
        let chrome_visible = chrome_visible.clone();
        Rc::new(move |show: bool| {
            chrome_visible.set(show);
            top_revealer.set_reveal_child(show);
            bottom_revealer.set_reveal_child(show);
        })
    };

    // Async load generation guard.
    let pos_cell = Rc::new(Cell::new(position));
    let load_gen = Rc::new(Cell::new(0u64));
    let load_into_picture = Rc::new({
        let ui = ui.clone();
        let loader_overlay = loader_overlay.clone();
        let unavailable_overlay = unavailable_overlay.clone();
        let unavailable_filename = unavailable_filename.clone();
        let unavailable_mime = unavailable_mime.clone();
        let unavailable_open = unavailable_open.clone();
        let unavailable_path = unavailable_path.clone();
        let load_gen = load_gen.clone();
        let pic_stack = pic_stack.clone();
        let active_a = active_a.clone();
        let video = video.clone();
        let ensure_video = ensure_video.clone();
        let media_stack = media_stack.clone();
        let set_video_input_active = set_video_input_active.clone();
        let video_controls_revealer = video_controls_revealer.clone();
        let video_reset_controls = video_reset_controls.clone();
        move |target: gtk::Picture,
              target_is_a: bool,
              asset_id: String,
              filename: String,
              mime: String,
              local_path: String,
              full_res: bool| {
            let ui = ui.clone();
            let loader = loader_overlay.clone();
            let unavailable_overlay = unavailable_overlay.clone();
            let unavailable_filename = unavailable_filename.clone();
            let unavailable_mime = unavailable_mime.clone();
            let unavailable_open = unavailable_open.clone();
            let unavailable_path = unavailable_path.clone();
            let load_gen = load_gen.clone();
            let pic_stack = pic_stack.clone();
            let active_a = active_a.clone();
            let video = video.clone();
            let ensure_video = ensure_video.clone();
            let media_stack = media_stack.clone();
            let set_video_input_active = set_video_input_active.clone();
            let video_controls_revealer = video_controls_revealer.clone();
            let video_reset_controls = video_reset_controls.clone();
            let lightbox_drag_path = lightbox_drag_path.clone();
            let our_gen = load_gen.get().wrapping_add(1);
            load_gen.set(our_gen);
            // Navigating (or reloading) always tears down any active playback so
            // the previous video stops and releases its decoder/network before
            // we show the next asset.
            if let Some(vp) = video.borrow().as_ref() {
                vp.stop();
            }
            if media_stack.visible_child_name().as_deref() == Some("video") {
                media_stack.set_visible_child_name("image");
            }
            // Reset to the still-image input model: release the video input layer
            // (so image gestures reach `scrolled_picture`) and hide/reset the
            // video playback controls until this asset proves to be a video.
            set_video_input_active(false);
            video_controls_revealer.set_reveal_child(false);
            video_reset_controls();
            unavailable_overlay.set_reveal_child(false);
            unavailable_overlay.set_can_target(false);
            *unavailable_path.borrow_mut() = None;
            *lightbox_drag_path.borrow_mut() = None;
            let is_video =
                crate::media_kinds::asset_kind(&mime) == crate::media_kinds::AssetKind::Video;
            // Fast path: if the preview texture is already in memory, paint it
            // AND flip the picture stack to it synchronously — right now, before
            // any `await`. This is what makes navigation feel instant: the slide
            // transition plays immediately with the cached image instead of
            // waiting for the async `load_thumbnail` round-trip (the ~0.5s lag).
            // The async block below still runs to (re)confirm the preview and,
            // for remote stills, upgrade to the original — but it no longer
            // gates when the new photo becomes visible.
            let mut committed_early = false;
            if let Some(texture) = ui
                .ctx
                .thumbnail_cache
                .get_cached(&asset_id, ThumbnailSize::Preview)
            {
                target.set_paintable(Some(&texture));
                pic_stack.set_visible_child_name(if target_is_a { "a" } else { "b" });
                active_a.set(target_is_a);
                committed_early = true;
            }
            let arm_delay_ms: u64 = if local_path.is_empty() { 120 } else { 250 };
            let loader_for_arm = loader.clone();
            let cancel_loader = Rc::new(Cell::new(false));
            let cancel_for_arm = cancel_loader.clone();
            // Skip the loader spinner entirely when we already committed a cached
            // preview — there is nothing for the user to wait on.
            if !committed_early {
                glib::timeout_add_local(std::time::Duration::from_millis(arm_delay_ms), move || {
                    if !cancel_for_arm.get() {
                        loader_for_arm.set_reveal_child(true);
                    }
                    glib::ControlFlow::Break
                });
            }
            glib::MainContext::default().spawn_local(async move {
                let is_current = || load_gen.get() == our_gen;
                let commit_visible = || {
                    let pic_stack = pic_stack.clone();
                    let active_a = active_a.clone();
                    glib::idle_add_local_once(move || {
                        pic_stack.set_visible_child_name(if target_is_a { "a" } else { "b" });
                        active_a.set(target_is_a);
                    });
                };
                let show_unavailable = |path: Option<String>| {
                    unavailable_filename.set_label(&filename);
                    unavailable_mime.set_label(&mime);
                    unavailable_open.set_visible(path.is_some());
                    *unavailable_path.borrow_mut() = path;
                    unavailable_overlay.set_can_target(true);
                    unavailable_overlay.set_reveal_child(true);
                };
                if is_video {
                    let thumb_result = ui
                        .ctx
                        .thumbnail_cache
                        .load_thumbnail(&asset_id, ThumbnailSize::Preview)
                        .await;
                    if !is_current() {
                        return;
                    }
                    if let Ok(texture) = thumb_result {
                        target.set_paintable(Some(&texture));
                    }
                    commit_visible();
                    cancel_loader.set(true);
                    loader.set_reveal_child(false);
                    let uri = if !local_path.is_empty() {
                        // Local file: GStreamer plays a file:// URI directly.
                        Some(format!("file://{}", local_path))
                    } else {
                        ui.ctx.api_client.video_playback_uri(&asset_id).await
                    };
                    if !is_current() {
                        return;
                    }
                    if let Some(uri) = uri {
                        // Build (or reuse) the GStreamer pipeline. If the sink
                        // plugin is missing, `ensure_video` returns None and we
                        // stay on the poster (already committed above).
                        if ensure_video().is_some()
                            && let Some(vp) = video.borrow().as_ref()
                        {
                            vp.play_uri(&uri);
                            media_stack.set_visible_child_name("video");
                            // Playing: raise the transparent input layer so
                            // tap+swipe reach it above the GL video surface,
                            // reset the playback controls, and mark them as the
                            // video-kind controls (revealed with the chrome).
                            set_video_input_active(true);
                            video_reset_controls();
                            video_controls_revealer.set_reveal_child(true);
                        }
                    }
                    return;
                }
                if !local_path.is_empty() {
                    if let Some(texture) =
                        load_texture_oriented(std::path::Path::new(&local_path)).await
                    {
                        if is_current() {
                            target.set_paintable(Some(&texture));
                            *lightbox_drag_path.borrow_mut() =
                                Some(std::path::PathBuf::from(&local_path));
                            commit_visible();
                            cancel_loader.set(true);
                            loader.set_reveal_child(false);
                        }
                        return;
                    }
                    if asset_id.starts_with(crate::library::LOCAL_ID_PREFIX) {
                        if is_current() {
                            show_unavailable(Some(local_path));
                            commit_visible();
                            cancel_loader.set(true);
                            loader.set_reveal_child(false);
                        }
                        return;
                    }
                }
                // Remote still image. Progressive display (Immich-mobile style):
                // (1) show the sharp full-preview (1440) immediately so a tapped
                // photo is instantly crisp, then (2) fetch + decode the ORIGINAL
                // in the background and swap it in for genuine full resolution.
                // Step 2 is non-blocking: the viewer is already interactive on
                // the preview, and navigating away (stale generation) cancels
                // the swap.
                let preview_result = ui
                    .ctx
                    .thumbnail_cache
                    .load_thumbnail(&asset_id, ThumbnailSize::Preview)
                    .await;
                if !is_current() {
                    return;
                }
                let mut have_image = false;
                match preview_result {
                    Ok(texture) => {
                        target.set_paintable(Some(&texture));
                        have_image = true;
                    }
                    Err(_) => show_unavailable(None),
                }
                commit_visible();
                if is_current() {
                    cancel_loader.set(true);
                    loader.set_reveal_child(false);
                }

                // Upgrade to the original for a genuinely full-resolution image.
                // `full_res` (the per-viewer pref) forces this even when the
                // preview failed; otherwise we only upgrade when the preview is
                // already showing, so a failed asset stays on the "unavailable"
                // card rather than double-erroring.
                if !have_image && !full_res {
                    return;
                }
                // Debounce the ORIGINAL fetch so it never rides the swipe's
                // critical path. The sharp 1440 preview is already on screen and
                // interactive; the full-res original is a slow network download +
                // large decode that also churns the shared download-session and
                // transfer-bar UI. During rapid prev/next swiping we must NOT kick
                // one of these per photo — that is exactly what janks navigation.
                // Wait for the user to settle (~350 ms); if they swiped on, the
                // load generation advanced and we bail before touching the network.
                glib::timeout_future(std::time::Duration::from_millis(350)).await;
                if !is_current() {
                    return;
                }
                if let Some(cache_dir) = crate::profile::cache_dir().map(|p| p.join("preview")) {
                    let _ = std::fs::create_dir_all(&cache_dir);
                    let temp = original_preview_cache_path(&cache_dir, &asset_id, &filename);
                    if !temp.exists() {
                        let download_result = {
                            begin_download_session(&ui.ctx, format!("preview {asset_id}"));
                            let progress = track_download_item(
                                &ui.ctx,
                                asset_id.clone(),
                                Some(format!("preview {asset_id}")),
                                None,
                            );
                            let result = ui
                                .ctx
                                .api_client
                                .download_original_to_file(&asset_id, &temp, Some(progress))
                                .await;
                            finish_download_item(&ui.ctx, &asset_id);
                            result
                        };
                        if let Err(err) = download_result {
                            log::warn!("Lightbox original fetch failed: {}", err);
                            // Keep the preview on screen if we already have one.
                            if is_current() && !have_image {
                                show_unavailable(None);
                                commit_visible();
                                cancel_loader.set(true);
                                loader.set_reveal_child(false);
                            }
                            return;
                        }
                    }
                    let decoded = load_texture_oriented(&temp).await;
                    if !is_current() {
                        return;
                    }
                    if let Some(texture) = decoded {
                        target.set_paintable(Some(&texture));
                        *lightbox_drag_path.borrow_mut() =
                            drag_export_path(&asset_id, &filename, &temp);
                    } else if !have_image {
                        show_unavailable(Some(temp.display().to_string()));
                        commit_visible();
                    }
                } else if is_current() && !have_image {
                    show_unavailable(None);
                    commit_visible();
                }
                if is_current() {
                    cancel_loader.set(true);
                    loader.set_reveal_child(false);
                }
            });
        }
    });

    // Full-resolution preference stored per-viewer; toggled via the ⋮ menu.
    let full_res_pref = Rc::new(Cell::new(resolution_full));
    // Current asset's remote id / favorite / description for the action wiring.
    let cur_favorite = Rc::new(Cell::new(false));

    // -1 = back/prev (slide right), +1 = forward/next (slide left), 0 = none.
    let nav_dir = Rc::new(Cell::new(0i8));
    let render = Rc::new({
        let ui = ui.clone();
        let pos_cell = pos_cell.clone();
        let load_into_picture = load_into_picture.clone();
        let full_res_pref = full_res_pref.clone();
        let pic_stack = pic_stack.clone();
        let media_dismiss_bin = media_dismiss_bin.clone();
        let picture_a = picture_a.clone();
        let picture_b = picture_b.clone();
        let scrolled_picture = scrolled_picture.clone();
        let active_a = active_a.clone();
        let zoom_level = zoom_level.clone();
        let nav_dir = nav_dir.clone();
        let datetime_label = datetime_label.clone();
        let favorite_btn = favorite_btn.clone();
        let cur_favorite = cur_favorite.clone();
        let details_timestamp = details_timestamp.clone();
        let description_row = description_row.clone();
        let details_map_box = details_map_box.clone();
        let details_exif = details_exif.clone();
        let details_people = details_people.clone();
        let details_people_row = details_people_row.clone();
        let details_loading = details_loading.clone();
        move || {
            let pos = pos_cell.get();
            let Some(item) = ui.grid.model.item(pos).and_downcast::<AssetObject>() else {
                return;
            };
            let asset_id = item.property::<String>("id");
            let filename = item.property::<String>("filename");
            let local_path = item.property::<String>("local-path");
            let mime = item.property::<String>("mime-type");
            let created = item.property::<String>("created-at");

            datetime_label.set_label(&format_topbar_datetime(&created));
            details_timestamp.set_label(&format_full_timestamp(&created));

            // Reset per-asset details UI.
            while let Some(c) = details_exif.first_child() {
                details_exif.remove(&c);
            }
            details_exif.set_visible(false);
            while let Some(c) = details_map_box.first_child() {
                details_map_box.remove(&c);
            }
            // Clear + hide the people strip so a remote→local (or empty)
            // navigation never leaves stale faces from the previous asset.
            while let Some(c) = details_people_row.first_child() {
                details_people_row.remove(&c);
            }
            details_people.set_visible(false);
            description_row.set_text("");
            favorite_btn.set_active(false);
            favorite_btn.set_icon_name("non-starred-symbolic");
            cur_favorite.set(false);

            let is_local = !local_path.is_empty() && asset_id.starts_with(LOCAL_ID_PREFIX);
            let is_video =
                crate::media_kinds::asset_kind(&mime) == crate::media_kinds::AssetKind::Video;

            // Load into the inactive picture; commit the slide once ready.
            let target_is_a = !active_a.get();
            let target = if target_is_a {
                picture_a.clone()
            } else {
                picture_b.clone()
            };
            zoom_level.set(1.0);
            apply_lightbox_zoom(&target, &scrolled_picture, 1.0);
            // Clear any leftover drag-to-dismiss translation/opacity so the new
            // photo is centered and fully opaque. Reset the OUTER bin (the one
            // the swipe gestures drive, covering both image and video).
            media_dismiss_bin.reset_dismiss();
            pic_stack.set_transition_type(match nav_dir.get() {
                1 => gtk::StackTransitionType::SlideLeft,
                -1 => gtk::StackTransitionType::SlideRight,
                _ => gtk::StackTransitionType::None,
            });
            // Videos and local files always use their native source; the
            // full-resolution preference only affects remote still images.
            let want_full = full_res_pref.get() && !is_local && !is_video;
            (*load_into_picture)(
                target,
                target_is_a,
                asset_id.clone(),
                filename.clone(),
                mime.clone(),
                local_path.clone(),
                want_full,
            );
            nav_dir.set(0);

            if is_local && !is_video {
                // Local image: parse EXIF on a blocking worker.
                details_loading.set_visible(true);
                let pos_cell_async = pos_cell.clone();
                let details_loading = details_loading.clone();
                let details_exif = details_exif.clone();
                let details_map_box = details_map_box.clone();
                let local_path_async = local_path.clone();
                glib::MainContext::default().spawn_local(async move {
                    let cache_root = local_exif::cache_root();
                    let path_for_blocking = local_path_async.clone();
                    let probed = tokio::task::spawn_blocking(move || {
                        let path = std::path::Path::new(&path_for_blocking);
                        let meta = std::fs::metadata(path).ok();
                        let file_size = meta.as_ref().map(|m| m.len());
                        let mtime_iso = meta
                            .as_ref()
                            .and_then(|m| m.modified().ok())
                            .map(systemtime_to_rfc3339);
                        let dims = gtk::gdk_pixbuf::Pixbuf::file_info(path)
                            .map(|(_, w, h)| (w, h))
                            .and_then(|(w, h)| {
                                let w = u32::try_from(w).ok()?;
                                let h = u32::try_from(h).ok()?;
                                Some((w, h))
                            });
                        let exif = local_exif::load_or_extract(&cache_root, path);
                        LocalProbe {
                            exif,
                            file_size,
                            mtime_iso,
                            dims,
                        }
                    })
                    .await
                    .ok();
                    if pos_cell_async.get() != pos {
                        return;
                    }
                    details_loading.set_visible(false);
                    let Some(probe) = probed else {
                        return;
                    };
                    if probe.exif.is_none()
                        && probe.file_size.is_none()
                        && probe.dims.is_none()
                        && probe.mtime_iso.is_none()
                    {
                        return;
                    }
                    let mut info = probe
                        .exif
                        .as_ref()
                        .map_or_else(LocalExif::default, Clone::clone);
                    if info.image_width.is_none() {
                        info.image_width = probe.dims.map(|(w, _)| w);
                    }
                    if info.image_height.is_none() {
                        info.image_height = probe.dims.map(|(_, h)| h);
                    }
                    let used_mtime_fallback = info.date_time_original.is_none();
                    if used_mtime_fallback {
                        info.date_time_original = probe.mtime_iso.clone();
                    }
                    let taken_label = if used_mtime_fallback {
                        "Modified"
                    } else {
                        "Taken"
                    };
                    let projected = exif_from_local(&info, probe.file_size);
                    if let (Some(lat), Some(lon)) = (projected.latitude, projected.longitude)
                        && let Some(map) = build_minimap(lat, lon)
                    {
                        details_map_box.append(&map);
                    }
                    fill_exif_box(&details_exif, &projected, taken_label);
                    details_exif.set_visible(true);
                });
                return;
            }

            // Remote asset: fetch full details for EXIF, favorite, description.
            details_loading.set_visible(true);
            let pos_cell_async = pos_cell.clone();
            let ui_async = ui.clone();
            let details_loading = details_loading.clone();
            let details_exif = details_exif.clone();
            let details_map_box = details_map_box.clone();
            let details_people = details_people.clone();
            let details_people_row = details_people_row.clone();
            let description_row = description_row.clone();
            let favorite_btn = favorite_btn.clone();
            let cur_favorite = cur_favorite.clone();
            let asset_id_async = asset_id.clone();
            glib::MainContext::default().spawn_local(async move {
                let result = ui_async
                    .ctx
                    .api_client
                    .fetch_asset_details(&asset_id_async)
                    .await;
                if pos_cell_async.get() != pos {
                    return;
                }
                details_loading.set_visible(false);
                let Ok(details) = result else { return };
                favorite_btn.set_active(details.is_favorite);
                favorite_btn.set_icon_name(if details.is_favorite {
                    "starred-symbolic"
                } else {
                    "non-starred-symbolic"
                });
                cur_favorite.set(details.is_favorite);
                if let Some(desc) = &details.description {
                    description_row.set_text(desc);
                } else if let Some(exif) = &details.exif_info
                    && let Some(desc) = &exif.description
                {
                    description_row.set_text(desc);
                }
                if let Some(exif) = details.exif_info {
                    if let (Some(lat), Some(lon)) = (exif.latitude, exif.longitude)
                        && let Some(map) = build_minimap(lat, lon)
                    {
                        details_map_box.append(&map);
                    }
                    fill_exif_box(&details_exif, &exif, "Taken");
                    details_exif.set_visible(true);
                }

                // ── Recognized people/faces ──────────────────────────────
                // Show every non-hidden person (named or not). Each tile is a
                // circular avatar (initials fallback until the real thumbnail
                // arrives) over an ellipsized name label. The avatar
                // thumbnails load lazily and re-check the position guard so a
                // fast swipe never paints a face onto the wrong photo.
                let visible_people: Vec<crate::api_client::Person> = details
                    .people
                    .into_iter()
                    .filter(|p| !p.is_hidden)
                    .collect();
                if !visible_people.is_empty() {
                    for person in visible_people {
                        let tile = gtk::Box::builder()
                            .orientation(gtk::Orientation::Vertical)
                            .spacing(4)
                            .halign(gtk::Align::Center)
                            .build();
                        let avatar = libadwaita::Avatar::new(
                            48,
                            (!person.name.is_empty()).then_some(person.name.as_str()),
                            true,
                        );
                        tile.append(&avatar);
                        if !person.name.is_empty() {
                            let name_label = gtk::Label::builder()
                                .label(&person.name)
                                .ellipsize(gtk::pango::EllipsizeMode::End)
                                .max_width_chars(8)
                                .halign(gtk::Align::Center)
                                .css_classes(["caption"])
                                .build();
                            tile.append(&name_label);
                        }
                        details_people_row.append(&tile);

                        // Lazily fetch the real face thumbnail; drop the result
                        // if the user navigated away in the meantime.
                        let ctx = ui_async.ctx.clone();
                        let pos_cell_person = pos_cell_async.clone();
                        let person_id = person.id.clone();
                        glib::MainContext::default().spawn_local(async move {
                            let bytes = match ctx.api_client.fetch_person_thumbnail(&person_id).await
                            {
                                Ok(b) => b,
                                Err(_) => return,
                            };
                            if pos_cell_person.get() != pos {
                                return;
                            }
                            if let Ok(texture) = gtk::gdk::Texture::from_bytes(
                                &glib::Bytes::from(&bytes[..]),
                            ) {
                                avatar.set_custom_image(Some(&texture));
                            }
                        });
                    }
                    details_people.set_visible(true);
                }
            });
        }
    });

    (*render)();

    // ── Navigation helpers ──────────────────────────────────────────────
    let goto_prev = Rc::new(clone!(
        #[strong]
        pos_cell,
        #[strong]
        render,
        #[strong]
        nav_dir,
        #[strong]
        set_chrome,
        move || {
            let pos = pos_cell.get();
            if pos > 0 {
                pos_cell.set(pos - 1);
                nav_dir.set(-1);
                (*set_chrome)(false);
                (*render)();
            }
        }
    ));
    let goto_next = Rc::new(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        #[strong]
        render,
        #[strong]
        nav_dir,
        #[strong]
        set_chrome,
        move || {
            let pos = pos_cell.get();
            if pos + 1 < ui.grid.model.n_items() {
                pos_cell.set(pos + 1);
                nav_dir.set(1);
                (*set_chrome)(false);
                (*render)();
                return;
            }
            let next_request = ui.ctx.library_state.lock().load_next_page_if_needed();
            let Some(req) = next_request else {
                return;
            };
            let model = ui.grid.model.clone();
            let pos_cell_h = pos_cell.clone();
            let render_h = render.clone();
            let nav_dir_h = nav_dir.clone();
            let prev_count = model.n_items();
            let handler_id = Rc::new(std::cell::RefCell::new(None::<glib::SignalHandlerId>));
            let handler_id_clone = handler_id.clone();
            let id = model.connect_items_changed(move |m, _, _, _| {
                if m.n_items() <= prev_count {
                    return;
                }
                let pos = pos_cell_h.get();
                if pos + 1 < m.n_items() {
                    pos_cell_h.set(pos + 1);
                    nav_dir_h.set(1);
                    (*render_h)();
                }
                if let Some(hid) = handler_id_clone.borrow_mut().take() {
                    m.disconnect(hid);
                }
            });
            *handler_id.borrow_mut() = Some(id);
            load_source_page(ui.clone(), req, true);
        }
    ));

    // Exit the viewer (back button / Escape / delete). Plays AdwNavigationView's
    // normal (rightward) pop transition, which is what those entry points want.
    let exit_viewer = Rc::new(clone!(
        #[strong]
        ui,
        move || {
            ui.nav.pop();
        }
    ));

    // Exit used at the end of the swipe-down dismiss animation. By that point
    // the page is already translated fully off-screen-down at opacity 0, so we
    // must NOT let the NavigationView play its horizontal pop slide on top of
    // the downward motion. Suppress transitions across just this pop, then
    // restore them once the pop has fully settled so subsequent pushes/pops
    // animate normally.
    //
    // The restore uses a short *timeout*, not `idle_add`: an idle callback can
    // run inside the same frame in which AdwNavigationView would otherwise
    // schedule the pop's slide on the frame clock, re-enabling animation early
    // enough for the horizontal slide to flash. A 250 ms timeout keeps the flag
    // off across the entire pop frame(s) and only re-arms animation well after
    // the page is gone.
    let exit_viewer_dismiss = Rc::new(clone!(
        #[strong]
        ui,
        move || {
            let nav = ui.nav.clone();
            let was_animating = nav.is_animate_transitions();
            nav.set_animate_transitions(false);
            nav.pop();
            if was_animating {
                glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
                    nav.set_animate_transitions(true);
                });
            }
        }
    ));

    back_btn.connect_clicked(clone!(
        #[strong]
        exit_viewer,
        move |_| (*exit_viewer)()
    ));

    // Toggle chrome on single tap (no drag). GestureClick with n_press==1.
    //
    // Built by a closure so an IDENTICAL tap can be attached to BOTH the
    // `media_stack` (handles images and paused video) AND the transparent
    // `video_input_layer` above the video surface (handles playing video, which
    // otherwise swallows ancestor input via its GL subsurface — see the input
    // layer comment). Only one is ever the event target (the layer is pickable
    // only while a video shows), so the tap never fires twice.
    let attach_tap = {
        let set_chrome = set_chrome.clone();
        let chrome_visible = chrome_visible.clone();
        let zoom_level = zoom_level.clone();
        move |widget: &gtk::Widget| {
            let tap = gtk::GestureClick::new();
            tap.set_button(gtk::gdk::BUTTON_PRIMARY);
            tap.connect_released(clone!(
                #[strong]
                set_chrome,
                #[strong]
                chrome_visible,
                #[strong]
                zoom_level,
                move |gesture, n_press, _, _| {
                    // Ignore taps while zoomed in (those pan) and multi-press.
                    if n_press != 1 || (zoom_level.get() - 1.0).abs() > 0.01 {
                        return;
                    }
                    (*set_chrome)(!chrome_visible.get());
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
            ));
            widget.add_controller(tap);
        }
    };
    attach_tap(media_stack.upcast_ref::<gtk::Widget>());
    attach_tap(video_input_layer.upcast_ref::<gtk::Widget>());

    // Favorite toggle.
    favorite_btn.connect_clicked(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        #[strong]
        cur_favorite,
        move |btn| {
            let Some(item) = ui.grid.model.item(pos_cell.get()).and_downcast::<AssetObject>()
            else {
                return;
            };
            let asset_id = item.property::<String>("id");
            if asset_id.starts_with(LOCAL_ID_PREFIX) {
                return;
            }
            let want = !cur_favorite.get();
            cur_favorite.set(want);
            btn.set_icon_name(if want {
                "starred-symbolic"
            } else {
                "non-starred-symbolic"
            });
            let ui = ui.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Err(err) = ui.ctx.api_client.set_asset_favorite(&asset_id, want).await {
                    log::warn!("Set favorite failed: {}", err);
                }
            });
        }
    ));

    // Description commit.
    description_row.connect_apply(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        move |row| {
            let Some(item) = ui.grid.model.item(pos_cell.get()).and_downcast::<AssetObject>()
            else {
                return;
            };
            let asset_id = item.property::<String>("id");
            if asset_id.starts_with(LOCAL_ID_PREFIX) {
                return;
            }
            let text = row.text().to_string();
            let ui = ui.clone();
            glib::MainContext::default().spawn_local(async move {
                if let Err(err) = ui.ctx.api_client.set_asset_description(&asset_id, &text).await
                {
                    log::warn!("Set description failed: {}", err);
                }
            });
        }
    ));

    // Delete action (confirm → soft-delete → pop viewer + refresh).
    delete_btn.connect_clicked(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        #[strong]
        exit_viewer,
        move |_| {
            let Some(item) = ui.grid.model.item(pos_cell.get()).and_downcast::<AssetObject>()
            else {
                return;
            };
            let asset_id = item.property::<String>("id");
            if asset_id.starts_with(LOCAL_ID_PREFIX) {
                show_lightbox_alert(&ui, "Cannot delete", "This is a local-only asset.");
                return;
            }
            let dialog = libadwaita::AlertDialog::builder()
                .heading("Move to trash?")
                .body("The item can be restored from the Immich trash.")
                .build();
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("delete", "Move to trash");
            dialog.set_response_appearance("delete", libadwaita::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            let ui2 = ui.clone();
            let exit_viewer = exit_viewer.clone();
            dialog.connect_response(None, move |dlg, response| {
                dlg.close();
                if response != "delete" {
                    return;
                }
                let ui = ui2.clone();
                let exit_viewer = exit_viewer.clone();
                let ids = vec![asset_id.clone()];
                glib::MainContext::default().spawn_local(async move {
                    match ui.ctx.api_client.delete_assets(&ids).await {
                        Ok(()) => {
                            (*exit_viewer)();
                            super::refresh_library_after_mutation(ui.clone(), true);
                        }
                        Err(err) => {
                            log::error!("Delete failed: {}", err);
                            show_lightbox_alert(&ui, "Delete failed", &err);
                        }
                    }
                });
            });
            dialog.present(Some(&ui.window));
        }
    ));

    // Add-to-album action.
    addalbum_btn.connect_clicked(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        move |_| {
            let Some(item) = ui.grid.model.item(pos_cell.get()).and_downcast::<AssetObject>()
            else {
                return;
            };
            let asset_id = item.property::<String>("id");
            if asset_id.starts_with(LOCAL_ID_PREFIX) {
                show_lightbox_alert(&ui, "Cannot add", "This is a local-only asset.");
                return;
            }
            show_add_to_album_dialog(ui.clone(), asset_id);
        }
    ));

    // Share action: export the original then hand off to the portal / default.
    share_btn.connect_clicked(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        move |_| {
            let Some(item) = ui.grid.model.item(pos_cell.get()).and_downcast::<AssetObject>()
            else {
                return;
            };
            let asset_id = item.property::<String>("id");
            let remote_id = item.property::<String>("remote-id");
            let local_path = item.property::<String>("local-path");
            let filename = item.property::<String>("filename");
            let ui = ui.clone();
            glib::MainContext::default().spawn_local(async move {
                match super::context_menu::ensure_original_asset_path(
                    &ui, &asset_id, &remote_id, &local_path, &filename,
                )
                .await
                {
                    Ok(path) => open_local_with_default_app(&path.display().to_string()),
                    Err(err) => show_lightbox_alert(&ui, "Share failed", &err),
                }
            });
        }
    ));

    // ── Menu actions (download / copy / open-with / full-res) ───────────
    let action_group = gtk::gio::SimpleActionGroup::new();
    let download_action = gtk::gio::SimpleAction::new("download", None);
    download_action.connect_activate(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        move |_, _| {
            if let Some(item) = ui.grid.model.item(pos_cell.get()).and_downcast::<AssetObject>() {
                let asset_id = item.property::<String>("id");
                let filename = item.property::<String>("filename");
                if !asset_id.starts_with(LOCAL_ID_PREFIX) {
                    start_download(ui.clone(), asset_id, filename);
                }
            }
        }
    ));
    action_group.add_action(&download_action);
    let copy_action = gtk::gio::SimpleAction::new("copy", None);
    copy_action.connect_activate(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        move |_, _| {
            super::context_menu::copy_current_asset(ui.clone(), pos_cell.get());
        }
    ));
    action_group.add_action(&copy_action);
    let openwith_action = gtk::gio::SimpleAction::new("openwith", None);
    openwith_action.connect_activate(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        move |_, _| {
            super::context_menu::open_current_asset_in_default_app(ui.clone(), pos_cell.get());
        }
    ));
    action_group.add_action(&openwith_action);
    let fullres_action = gtk::gio::SimpleAction::new_stateful(
        "fullres",
        None,
        &full_res_pref.get().to_variant(),
    );
    fullres_action.connect_activate(clone!(
        #[strong]
        full_res_pref,
        #[strong]
        render,
        move |act, _| {
            let new = !full_res_pref.get();
            full_res_pref.set(new);
            act.set_state(&new.to_variant());
            (*render)();
        }
    ));
    action_group.add_action(&fullres_action);
    page.insert_action_group("lightbox", Some(&action_group));

    // ── Zoom / pan machinery ────────────────────────────────────────────
    let active_picture = clone!(
        #[strong]
        active_a,
        #[strong]
        picture_a,
        #[strong]
        picture_b,
        move || -> gtk::Picture {
            if active_a.get() {
                picture_a.clone()
            } else {
                picture_b.clone()
            }
        }
    );
    let cursor_pos: Rc<Cell<Option<(f64, f64)>>> = Rc::new(Cell::new(None));
    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion(clone!(
        #[strong]
        cursor_pos,
        move |_, x, y| {
            cursor_pos.set(Some((x, y)));
        }
    ));
    motion.connect_leave(clone!(
        #[strong]
        cursor_pos,
        move |_| {
            cursor_pos.set(None);
        }
    ));
    scrolled_picture.add_controller(motion);

    let set_zoom = Rc::new(clone!(
        #[strong]
        zoom_level,
        #[strong]
        active_picture,
        #[strong]
        scrolled_picture,
        #[strong]
        cursor_pos,
        move |z: f64| {
            let z_new = z.clamp(1.0, 10.0);
            let z_old = zoom_level.get();
            if (z_new - z_old).abs() < 0.0001 {
                return;
            }
            let viewer_w = scrolled_picture.width().max(1) as f64;
            let viewer_h = scrolled_picture.height().max(1) as f64;
            let (fx, fy) = cursor_pos
                .get()
                .filter(|&(x, y)| x >= 0.0 && y >= 0.0 && x <= viewer_w && y <= viewer_h)
                .unwrap_or((viewer_w / 2.0, viewer_h / 2.0));
            let hadj = scrolled_picture.hadjustment();
            let vadj = scrolled_picture.vadjustment();
            let scroll_x = hadj.value();
            let scroll_y = vadj.value();
            let ratio = z_new / z_old.max(0.0001);
            let target_scroll_x = (scroll_x + fx) * ratio - fx;
            let target_scroll_y = (scroll_y + fy) * ratio - fy;
            zoom_level.set(z_new);
            let content = apply_lightbox_zoom(&active_picture(), &scrolled_picture, z_new);
            if let Some((cw, ch)) = content {
                hadj.set_upper(cw.max(viewer_w));
                hadj.set_page_size(viewer_w);
                vadj.set_upper(ch.max(viewer_h));
                vadj.set_page_size(viewer_h);
            } else {
                hadj.set_upper(viewer_w);
                hadj.set_page_size(viewer_w);
                vadj.set_upper(viewer_h);
                vadj.set_page_size(viewer_h);
            }
            hadj.set_value(target_scroll_x);
            vadj.set_value(target_scroll_y);
        }
    ));
    let zoom_by = Rc::new(clone!(
        #[strong]
        zoom_level,
        #[strong]
        set_zoom,
        move |factor: f64| {
            (*set_zoom)(zoom_level.get() * factor);
        }
    ));
    let zoom_reset = Rc::new(clone!(
        #[strong]
        set_zoom,
        move || {
            (*set_zoom)(1.0);
        }
    ));

    // Trackpad pinch-to-zoom.
    let pinch = gtk::GestureZoom::new();
    let pinch_start = Rc::new(Cell::new(1.0_f64));
    pinch.connect_begin(clone!(
        #[strong]
        zoom_level,
        #[strong]
        pinch_start,
        move |_, _| {
            pinch_start.set(zoom_level.get());
        }
    ));
    pinch.connect_scale_changed(clone!(
        #[strong]
        pinch_start,
        #[strong]
        set_zoom,
        #[strong]
        swipe_active,
        #[strong]
        media_stack,
        move |_, scale| {
            // Video is not zoomable: no-op when the video child is showing so a
            // pinch over an inline video can never zoom it.
            if media_stack.visible_child_name().as_deref() == Some("video") {
                return;
            }
            // Never zoom while a single-finger directional swipe owns the
            // sequence. `GestureZoom` needs 2 touch points so this should never
            // fire on a 1-finger swipe, but guard anyway so a stray 2-point
            // jitter mid-swipe can't hijack the gesture into a zoom.
            if swipe_active.get() {
                return;
            }
            (*set_zoom)(pinch_start.get() * scale);
        }
    ));
    scrolled_picture.add_controller(pinch.clone());

    // Click-and-drag panning when zoomed in.
    let drag_start = Rc::new(Cell::new((0.0_f64, 0.0_f64)));
    let pan_drag = gtk::GestureDrag::new();
    pan_drag.set_button(gtk::gdk::BUTTON_PRIMARY);
    pan_drag.connect_drag_begin(clone!(
        #[strong]
        scrolled_picture,
        #[strong]
        drag_start,
        move |_, _, _| {
            let hadj = scrolled_picture.hadjustment();
            let vadj = scrolled_picture.vadjustment();
            drag_start.set((hadj.value(), vadj.value()));
        }
    ));
    pan_drag.connect_drag_update(clone!(
        #[strong]
        scrolled_picture,
        #[strong]
        drag_start,
        #[strong]
        zoom_level,
        #[strong]
        swipe_active,
        #[strong]
        media_stack,
        move |_, off_x, off_y| {
            // Video is not pannable: never touch scroll when the video child is
            // showing so a drag over an inline video is free to navigate/dismiss
            // via `nav_drag`. (Zoom is also forced to 1.0 over video, but guard
            // explicitly.)
            if media_stack.visible_child_name().as_deref() == Some("video") {
                return;
            }
            // Pan ONLY when zoomed in; at fit (zoom == 1.0) leave scroll alone so
            // the `nav_drag` swipe gesture owns the motion. Also stand down while
            // a directional swipe/dismiss is in flight so a claimed swipe can
            // never be reinterpreted as a pan.
            if swipe_active.get() || (zoom_level.get() - 1.0).abs() < 0.01 {
                return;
            }
            let (sx0, sy0) = drag_start.get();
            scrolled_picture.hadjustment().set_value(sx0 - off_x);
            scrolled_picture.vadjustment().set_value(sy0 - off_y);
        }
    ));
    scrolled_picture.add_controller(pan_drag.clone());
    pan_drag.group_with(&pinch);

    // Double-tap: zoom in 2x toward the tap position (or back out).
    //
    // This must fire only on a genuine double *tap* — never on a swipe. Two
    // problems the naive `connect_pressed(n_press == 2)` had:
    //   (1) two quick nav swipes land two presses inside GestureClick's
    //       double-click window, so the second swipe's press reads as
    //       `n_press == 2` and zooms mid-swipe;
    //   (2) it fired on press, before we know whether the finger will travel.
    // Fix: act on `released`, require the press→release displacement to be
    // tiny (a real tap, not a drag), and refuse entirely while a directional
    // swipe/dismiss is in flight (`swipe_active`). We remember the first
    // press position so the zoom re-centres on where the user tapped.
    let double_click = gtk::GestureClick::new();
    double_click.set_button(gtk::gdk::BUTTON_PRIMARY);
    let dbl_press_pos = Rc::new(Cell::new((0.0_f64, 0.0_f64)));
    double_click.connect_pressed(clone!(
        #[strong]
        dbl_press_pos,
        move |_, _, x, y| {
            dbl_press_pos.set((x, y));
        }
    ));
    double_click.connect_released(clone!(
        #[strong]
        cursor_pos,
        #[strong]
        zoom_level,
        #[strong]
        set_zoom,
        #[strong]
        swipe_active,
        #[strong]
        dbl_press_pos,
        #[strong]
        media_stack,
        move |_, n_press, x, y| {
            // Video is not zoomable: a double-tap over an inline video must not
            // zoom (it just does nothing extra beyond the tap's chrome toggle).
            if media_stack.visible_child_name().as_deref() == Some("video") {
                return;
            }
            if n_press != 2 || swipe_active.get() {
                return;
            }
            // Reject if the finger moved appreciably between press and release
            // — that's a drag/swipe, not a tap.
            let (px, py) = dbl_press_pos.get();
            if (x - px).hypot(y - py) > 12.0 {
                return;
            }
            cursor_pos.set(Some((x, y)));
            let z = zoom_level.get();
            if (z - 1.0).abs() < 0.01 {
                (*set_zoom)(2.0);
            } else {
                (*set_zoom)(1.0);
            }
        }
    ));
    scrolled_picture.add_controller(double_click);

    // Right-click: open the standard asset context menu.
    let right_click = gtk::GestureClick::new();
    right_click.set_button(gtk::gdk::BUTTON_SECONDARY);
    right_click.connect_pressed(clone!(
        #[strong]
        ui,
        #[strong]
        pos_cell,
        #[strong]
        scrolled_picture,
        move |_, _, x, y| {
            show_asset_context_menu(ui.clone(), &scrolled_picture, pos_cell.get(), x, y);
        }
    ));
    scrolled_picture.add_controller(right_click);

    // ── Directional swipe/drag gesture ──────────────────────────────────
    // A CAPTURE-phase drag on the picture stack that interprets the release
    // direction: horizontal = prev/next, down = dismiss, up = details sheet.
    // Attaching at capture and claiming the sequence stops the enclosing
    // AdwNavigationView from treating a horizontal drag as an edge-swipe pop.
    //
    // Built by a closure so an IDENTICAL directional gesture can be attached to
    // BOTH `media_stack` (images + paused video) AND the transparent
    // `video_input_layer` above the playing-video GL surface (which swallows
    // ancestor input). The state cells and helper closures below are SHARED
    // between the two gestures — only one is ever the event target at a time (the
    // layer is pickable only while a video shows), so they never both drive a
    // drag simultaneously.
    let drag_claimed = Rc::new(Cell::new(false));
    // Tracks whether the *current* drag is an interactive drag-to-dismiss
    // (downward-dominant): once set, `drag_update` translates the image with
    // the finger and `drag_end` decides dismiss-vs-snap-back.
    let drag_dismissing = Rc::new(Cell::new(false));
    // Tracks whether the *current* drag is a live horizontal prev/next swipe
    // (horizontal-dominant): once set, `drag_update` translates the image
    // sideways with the finger (finger-following) and `drag_end` decides
    // navigate-vs-snap-back.
    let drag_horizontal = Rc::new(Cell::new(false));
    // Rolling velocity estimate (px/s, downward positive) for flick dismissal.
    // Sampled from `(offset, Instant)` deltas in `drag_update` so it's ready in
    // `drag_end` regardless of any parallel gesture's callback ordering.
    let drag_velocity_y = Rc::new(Cell::new(0.0_f64));
    let drag_last_sample: Rc<Cell<Option<(f64, std::time::Instant)>>> = Rc::new(Cell::new(None));
    // Applies a live downward translation + fade to the *whole media stack*
    // (image OR video) via the outer GPU-transform bin (queue_draw only — no
    // relayout, so it stays smooth). Driving the outer bin is what makes the
    // dismiss motion visible over a playing video, not just over a photo.
    let apply_dismiss_offset = Rc::new(clone!(
        #[strong]
        media_dismiss_bin,
        #[strong]
        scrolled_picture,
        move |off_y: f64| {
            let h = scrolled_picture.height().max(1) as f64;
            media_dismiss_bin.set_dismiss_offset(off_y, h);
        }
    ));
    // Applies a live horizontal translation to the media stack for
    // finger-following prev/next navigation (queue_draw only — GPU transform,
    // no relayout).
    let apply_nav_offset = Rc::new(clone!(
        #[strong]
        media_dismiss_bin,
        move |off_x: f64| {
            media_dismiss_bin.set_nav_offset(off_x);
        }
    ));
    // Resets the translation/opacity so a freshly loaded photo is centered.
    let reset_dismiss_offset = Rc::new(clone!(
        #[strong]
        media_dismiss_bin,
        move || {
            media_dismiss_bin.reset_dismiss();
        }
    ));
    // Builds a fresh capture-phase directional gesture wired to the shared
    // state/helpers above and attaches it to `widget`. Called for both
    // `media_stack` and `video_input_layer`.
    let attach_nav_drag = {
        let goto_prev = goto_prev.clone();
        let goto_next = goto_next.clone();
        let exit_viewer_dismiss = exit_viewer_dismiss.clone();
        let sheet = sheet.clone();
        let zoom_level = zoom_level.clone();
        let drag_claimed = drag_claimed.clone();
        let drag_dismissing = drag_dismissing.clone();
        let drag_horizontal = drag_horizontal.clone();
        let swipe_active = swipe_active.clone();
        let drag_velocity_y = drag_velocity_y.clone();
        let drag_last_sample = drag_last_sample.clone();
        let media_dismiss_bin = media_dismiss_bin.clone();
        let scrolled_picture = scrolled_picture.clone();
        let apply_dismiss_offset = apply_dismiss_offset.clone();
        let apply_nav_offset = apply_nav_offset.clone();
        let reset_dismiss_offset = reset_dismiss_offset.clone();
        move |widget: &gtk::Widget| {
    let nav_drag = gtk::GestureDrag::new();
    nav_drag.set_touch_only(false);
    nav_drag.set_propagation_phase(gtk::PropagationPhase::Capture);
    nav_drag.connect_drag_begin(clone!(
        #[strong]
        drag_velocity_y,
        #[strong]
        drag_last_sample,
        #[strong]
        drag_claimed,
        #[strong]
        drag_dismissing,
        #[strong]
        drag_horizontal,
        #[strong]
        swipe_active,
        move |_, _, _| {
            drag_velocity_y.set(0.0);
            drag_last_sample.set(Some((0.0, std::time::Instant::now())));
            drag_claimed.set(false);
            drag_dismissing.set(false);
            drag_horizontal.set(false);
            swipe_active.set(false);
        }
    ));
    nav_drag.connect_drag_update(clone!(
        #[strong]
        zoom_level,
        #[strong]
        drag_claimed,
        #[strong]
        drag_dismissing,
        #[strong]
        drag_horizontal,
        #[strong]
        swipe_active,
        #[strong]
        drag_velocity_y,
        #[strong]
        drag_last_sample,
        #[strong]
        apply_dismiss_offset,
        #[strong]
        apply_nav_offset,
        move |gesture, off_x, off_y| {
            // When zoomed in the pan gesture owns the motion.
            if (zoom_level.get() - 1.0).abs() > 0.01 {
                return;
            }
            // Sample instantaneous vertical velocity (EMA-smoothed) so a quick
            // flick registers even with little travel.
            let now = std::time::Instant::now();
            if let Some((prev_y, prev_t)) = drag_last_sample.get() {
                let dt = now.duration_since(prev_t).as_secs_f64();
                if dt > 0.0005 {
                    let inst = (off_y - prev_y) / dt;
                    let prev_v = drag_velocity_y.get();
                    // 0.5 EMA: responsive but not jittery.
                    drag_velocity_y.set(prev_v * 0.5 + inst * 0.5);
                    drag_last_sample.set(Some((off_y, now)));
                }
            } else {
                drag_last_sample.set(Some((off_y, now)));
            }
            if !drag_claimed.get() {
                // Claim as soon as the motion is unambiguously a directional
                // drag. Claiming early (small threshold) is what makes the swipe
                // WIN the gesture negotiation: `set_state(Claimed)` denies the
                // same sequence to the bubble-phase zoom/pan gestures on the
                // ScrolledWindow (`pinch`/`pan_drag`/`double_click`), and setting
                // `swipe_active` makes them stand down defensively too. A lower
                // threshold than the double-tap slop (12px) guarantees a swipe is
                // owned by nav before any tap/zoom can act on it.
                if off_x.abs() > 16.0 || off_y.abs() > 16.0 {
                    drag_claimed.set(true);
                    swipe_active.set(true);
                    // Classify the drag once, at claim time: a downward-dominant
                    // drag is an interactive dismiss; an otherwise horizontal
                    // drag is a live prev/next swipe. Ties (diagonal) resolve to
                    // dismiss so an ambiguous down-ish drag never navigates.
                    if off_y.abs() >= off_x.abs() {
                        // Only treat as dismiss when actually heading DOWN; an
                        // upward drag falls through to the sheet-open branch in
                        // drag_end (no live translation needed for that).
                        if off_y > 0.0 {
                            drag_dismissing.set(true);
                        }
                    } else {
                        drag_horizontal.set(true);
                    }
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
            }
            // While dismissing, move the image down with the finger and fade.
            if drag_dismissing.get() {
                apply_dismiss_offset(off_y);
            } else if drag_horizontal.get() {
                // Finger-following horizontal translation. Dampen slightly so it
                // reads as elastic and doesn't run fully off-screen before the
                // snap animation takes over.
                apply_nav_offset(off_x * 0.9);
            }
        }
    ));
    nav_drag.connect_drag_end(clone!(
        #[strong]
        goto_prev,
        #[strong]
        goto_next,
        #[strong]
        exit_viewer_dismiss,
        #[strong]
        sheet,
        #[strong]
        zoom_level,
        #[strong]
        drag_claimed,
        #[strong]
        drag_dismissing,
        #[strong]
        drag_horizontal,
        #[strong]
        swipe_active,
        #[strong]
        drag_velocity_y,
        #[strong]
        drag_last_sample,
        #[strong]
        media_dismiss_bin,
        #[strong]
        scrolled_picture,
        #[strong]
        apply_dismiss_offset,
        #[strong]
        apply_nav_offset,
        #[strong]
        reset_dismiss_offset,
        move |_, off_x, off_y| {
            let _ = drag_claimed.replace(false);
            let was_dismissing = drag_dismissing.replace(false);
            let was_horizontal = drag_horizontal.replace(false);
            let velocity_y = drag_velocity_y.replace(0.0);
            drag_last_sample.set(None);
            // The directional swipe is over; let the zoom/pan gestures resume.
            swipe_active.set(false);
            if (zoom_level.get() - 1.0).abs() > 0.01 {
                return;
            }
            const THRESH: f64 = 60.0;
            const DISMISS_THRESH: f64 = 120.0;
            // Downward flick: dismiss even on little travel if fast enough.
            const FLICK_VELOCITY: f64 = 900.0;
            let ax = off_x.abs();
            let ay = off_y.abs();

            if was_dismissing {
                // Interactive downward dismiss. Dismiss when dragged past the
                // distance threshold OR flicked down fast; otherwise snap back.
                let h = scrolled_picture.height().max(1) as f64;
                let flick = velocity_y > FLICK_VELOCITY && off_y > 8.0;
                if off_y > DISMISS_THRESH || flick {
                    // Continue downward + fade, then pop. Faster if flicked.
                    let duration = if flick { 130 } else { 160 };
                    let target = CallbackAnimationTarget::new(clone!(
                        #[strong]
                        apply_dismiss_offset,
                        move |v| apply_dismiss_offset(v)
                    ));
                    let anim = TimedAnimation::new(
                        &media_dismiss_bin,
                        off_y.max(0.0),
                        h,
                        duration,
                        target,
                    );
                    anim.set_easing(libadwaita::Easing::EaseInCubic);
                    anim.connect_done(clone!(
                        #[strong]
                        exit_viewer_dismiss,
                        #[strong]
                        reset_dismiss_offset,
                        move |_| {
                            // Pop while the page is still fully off-screen-down at
                            // opacity 0 and with NavigationView transitions
                            // suppressed, so the only motion the user sees is the
                            // downward dismiss — no rightward pop slide. Reset the
                            // GPU offset only AFTER the pop (next idle), never
                            // before, so the page can't flash back to centre.
                            (*exit_viewer_dismiss)();
                            let reset_dismiss_offset = reset_dismiss_offset.clone();
                            glib::idle_add_local_once(move || {
                                reset_dismiss_offset();
                            });
                        }
                    ));
                    anim.play();
                } else {
                    // Snap back: animate the offset (and thus opacity) to 0.
                    let target = CallbackAnimationTarget::new(clone!(
                        #[strong]
                        apply_dismiss_offset,
                        move |v| apply_dismiss_offset(v)
                    ));
                    let anim =
                        TimedAnimation::new(&media_dismiss_bin, off_y.max(0.0), 0.0, 200, target);
                    anim.set_easing(libadwaita::Easing::EaseOutCubic);
                    anim.connect_done(clone!(
                        #[strong]
                        reset_dismiss_offset,
                        move |_| reset_dismiss_offset()
                    ));
                    anim.play();
                }
                return;
            }

            if was_horizontal {
                // Live finger-following horizontal swipe. Navigate past the
                // threshold, else spring the image back to centre. `goto_*`
                // re-renders the stack with a SlideLeft/SlideRight transition,
                // and `render` zeroes the offset, so the incoming photo slides
                // in from the drag position — no visible snap-to-centre first.
                const NAV_THRESH: f64 = 60.0;
                if ax > NAV_THRESH {
                    if off_x > 0.0 {
                        (*goto_prev)();
                    } else {
                        (*goto_next)();
                    }
                } else {
                    // Snap back to centre.
                    let target = CallbackAnimationTarget::new(clone!(
                        #[strong]
                        apply_nav_offset,
                        move |v| apply_nav_offset(v)
                    ));
                    let anim =
                        TimedAnimation::new(&media_dismiss_bin, off_x * 0.9, 0.0, 180, target);
                    anim.set_easing(libadwaita::Easing::EaseOutCubic);
                    anim.connect_done(clone!(
                        #[strong]
                        reset_dismiss_offset,
                        move |_| reset_dismiss_offset()
                    ));
                    anim.play();
                }
                return;
            }

            if ax < THRESH && ay < THRESH {
                return;
            }
            if ax > ay {
                // Horizontal (fallback when the drag wasn't classified live,
                // e.g. a quick flick): right drag → prev, left drag → next.
                if off_x > 0.0 {
                    (*goto_prev)();
                } else {
                    (*goto_next)();
                }
            } else if off_y < 0.0 {
                // Upward drag → open details sheet.
                sheet.set_open(true);
            }
            // Downward drags are handled by the interactive-dismiss branch above.
        }
    ));
    widget.add_controller(nav_drag);
        }
    };
    // Attach at the media_stack level so the directional swipe (navigate /
    // swipe-down dismiss / swipe-up details) fires over the image and paused
    // video. Capture phase + claiming still wins negotiation against the
    // enclosing AdwNavigationView edge-swipe and, over images, against the
    // zoom/pan gestures on the inner ScrolledWindow. Also attach an identical
    // gesture to the transparent `video_input_layer` so the swipe works over a
    // PLAYING video whose GL subsurface would otherwise swallow the input.
    attach_nav_drag(media_stack.upcast_ref::<gtk::Widget>());
    attach_nav_drag(video_input_layer.upcast_ref::<gtk::Widget>());

    // ── Keyboard shortcuts ──────────────────────────────────────────────
    let key_controller = gtk::EventControllerKey::new();
    key_controller.connect_key_pressed(clone!(
        #[strong]
        exit_viewer,
        #[strong]
        goto_prev,
        #[strong]
        goto_next,
        #[strong]
        zoom_by,
        #[strong]
        zoom_reset,
        #[strong]
        set_chrome,
        #[strong]
        chrome_visible,
        #[strong]
        sheet,
        move |_, key, _, mods| {
            let ctrl = mods.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            match (ctrl, key) {
                (true, gtk::gdk::Key::plus)
                | (true, gtk::gdk::Key::equal)
                | (true, gtk::gdk::Key::KP_Add) => {
                    (*zoom_by)(1.2);
                    glib::Propagation::Stop
                }
                (true, gtk::gdk::Key::minus) | (true, gtk::gdk::Key::KP_Subtract) => {
                    (*zoom_by)(1.0 / 1.2);
                    glib::Propagation::Stop
                }
                (true, gtk::gdk::Key::_0) | (true, gtk::gdk::Key::KP_0) => {
                    (*zoom_reset)();
                    glib::Propagation::Stop
                }
                (false, gtk::gdk::Key::Left) => {
                    (*goto_prev)();
                    glib::Propagation::Stop
                }
                (false, gtk::gdk::Key::Right) => {
                    (*goto_next)();
                    glib::Propagation::Stop
                }
                (false, gtk::gdk::Key::Up) | (false, gtk::gdk::Key::i) | (false, gtk::gdk::Key::I) => {
                    sheet.set_open(true);
                    glib::Propagation::Stop
                }
                (false, gtk::gdk::Key::space) => {
                    (*set_chrome)(!chrome_visible.get());
                    glib::Propagation::Stop
                }
                (false, gtk::gdk::Key::Escape) => {
                    (*exit_viewer)();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        }
    ));
    page.add_controller(key_controller);

    // Ctrl+wheel zoom on the picture area.
    let zoom_scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    zoom_scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    zoom_scroll.connect_scroll(clone!(
        #[strong]
        zoom_by,
        move |ctrl, dx, dy| {
            let mods = ctrl.current_event_state();
            if !mods.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                return glib::Propagation::Proceed;
            }
            let delta = if dy != 0.0 { dy } else { dx };
            if delta == 0.0 {
                return glib::Propagation::Proceed;
            }
            let factor = if delta < 0.0 { 1.1 } else { 1.0 / 1.1 };
            (*zoom_by)(factor);
            glib::Propagation::Stop
        }
    ));
    scrolled_picture.add_controller(zoom_scroll);

    ui.nav.push(&page);
}

#[cfg(test)]
mod tests {
    use super::original_preview_cache_path;

    #[test]
    fn original_preview_cache_path_keeps_decoder_extension() {
        let cache = std::path::Path::new("/tmp/previews");
        assert_eq!(
            original_preview_cache_path(cache, "remote-id", "PXL_20250516_222429137.dng"),
            cache.join("remote-id.dng")
        );
        assert_eq!(
            original_preview_cache_path(cache, "remote-id", "extensionless"),
            cache.join("remote-id.bin")
        );
    }
}
