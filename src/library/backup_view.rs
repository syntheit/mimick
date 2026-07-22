//! Immich-mobile-style photo backup home + folder-selection pages.
//!
//! The backup UX is assembled on top of the existing sync/upload engine: the
//! folder *selection* reuses `config.data.watch_paths` (no new store), the
//! counters reuse `enumerate_local` + `sync_index.fresh_checksum` +
//! `api_client.bulk_existing_asset_ids`, and "Back up now" reuses
//! `upload_picker::spawn_enqueue_with_callback`. Nothing here writes new upload
//! or network code — this module is pure UI surface.
//!
//! Both pages are full-screen `NavigationPage`s pushed onto the *root*
//! `NavigationView` (`ui.nav`, the same nav the lightbox uses) so they cover
//! the main header + bottom nav and carry their own `ToolbarView` + `HeaderBar`
//! with an automatic back button.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use glib::clone;
use gtk::prelude::*;
use libadwaita::prelude::*;

use super::LibraryWindowUi;
use crate::app_context::AppContext;
use crate::config::WatchPathEntry;

/// Auto-backup scheduler tick interval. Chosen to be frequent enough to feel
/// prompt while the app is open, but coarse enough to avoid battery churn on a
/// mobile device. The timeout persists for the app lifetime and dies with the
/// process when the window close → `app.quit()` path fires.
const BACKUP_TICK_SECS: u32 = 120;

/// Per-page cache of loaded upload-details thumbnails, keyed by item file path.
/// Threaded through the status tick so the rows (rebuilt each tick) don't
/// re-decode the same file repeatedly.
type ThumbCache = Rc<std::cell::RefCell<std::collections::HashMap<String, gtk::gdk::Texture>>>;

/// Build the "Backup" home page and push it onto the root navigation view.
///
/// The page is an `AdwPreferencesPage` (boxed list) wrapped in its own
/// `ToolbarView`/`HeaderBar`, matching IMG_0121: a "Backup folders" row, three
/// counter rows (Total / Backed up / Remaining), an "Enable backup" switch, and
/// a "Back up now" button. Counters are computed asynchronously off the main
/// thread and filled in when they resolve.
pub fn present_backup(ui: Rc<LibraryWindowUi>) {
    let page = build_backup_home(ui.clone());
    ui.nav.push(&page);
    // Opening the backup page is a natural moment to re-probe the server so the
    // grid's cloud badges (and, on return, the counters) reflect anything a
    // concurrent client changed. Runs off-thread; re-renders the grid when done.
    super::spawn_server_checksum_refresh(ui);
}

/// Construct the backup home page (§2).
fn build_backup_home(ui: Rc<LibraryWindowUi>) -> libadwaita::NavigationPage {
    let prefs = libadwaita::PreferencesPage::new();

    // --- Backup folders group ---------------------------------------------
    let folders_group = libadwaita::PreferencesGroup::builder()
        .title("Backup folders")
        .build();

    let folders_row = libadwaita::ActionRow::builder()
        .title("Folders to back up")
        .subtitle(&selected_folders_subtitle(&ui))
        .subtitle_lines(2)
        .build();
    let select_button = gtk::Button::builder()
        .label("Select")
        .valign(gtk::Align::Center)
        .css_classes(["mimick-pressable"])
        .build();
    folders_row.add_suffix(&select_button);
    folders_row.set_activatable_widget(Some(&select_button));
    folders_group.add(&folders_row);
    prefs.add(&folders_group);

    // --- Counters group ----------------------------------------------------
    let counters_group = libadwaita::PreferencesGroup::builder().build();
    let (total_row, total_label) =
        counter_row("Total", "All unique photos and videos in the selected folders");
    let (backed_row, backed_label) =
        counter_row("Backed up", "Photos and videos already on the server");
    let (remaining_row, remaining_label) = counter_row(
        "Remaining",
        "Photos and videos still to back up from the selection",
    );
    counters_group.add(&total_row);
    counters_group.add(&backed_row);
    counters_group.add(&remaining_row);
    prefs.add(&counters_group);

    // --- Live upload status group -----------------------------------------
    // Reflects the same `TransferSnapshot` the header backup icon uses. Hidden
    // while idle; shown with a summary row + overall bar + a per-item list while
    // an upload session is running. Driven by a `glib::timeout_add_local` tick
    // that self-cancels once the page is dropped (see below).
    let status_group = libadwaita::PreferencesGroup::builder()
        .title("Backing up")
        .visible(false)
        .build();
    let status_row = libadwaita::ActionRow::builder()
        .title("Uploading")
        .subtitle("")
        .build();
    let status_progress = gtk::ProgressBar::builder()
        .valign(gtk::Align::Center)
        .hexpand(true)
        .width_request(96)
        .build();
    status_row.add_suffix(&status_progress);
    status_group.add(&status_row);
    let items_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .visible(false)
        .build();
    status_group.add(&items_list);
    prefs.add(&status_group);
    // One-shot guard so re-`shown` events (e.g. returning from the folder-select
    // sub-page) don't stack duplicate status-tick timers on the same widgets.
    let tick_started = Rc::new(std::cell::Cell::new(false));
    // Per-page thumbnail cache for the upload-details rows, keyed by the item's
    // file path. The rows are rebuilt every tick, so caching the loaded textures
    // here keeps us from re-issuing an async decode for the same file each tick.
    let thumb_cache: ThumbCache = Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));

    // --- Enable + action group --------------------------------------------
    let toggle_group = libadwaita::PreferencesGroup::builder().build();
    let enable_row = libadwaita::SwitchRow::builder()
        .title("Enable backup")
        .subtitle("Automatically back up the selected folders")
        .active(ui.ctx.config.read().data.backup_enabled)
        .build();
    enable_row.connect_active_notify(clone!(
        #[strong]
        ui,
        move |row| set_backup_enabled(&ui, row.is_active())
    ));
    toggle_group.add(&enable_row);

    let backup_now_button = gtk::Button::builder()
        .label("Back up now")
        .halign(gtk::Align::Center)
        .margin_top(12)
        .margin_bottom(4)
        .css_classes(["pill", "suggested-action", "mimick-pressable"])
        .build();
    toggle_group.add(&backup_now_button);
    prefs.add(&toggle_group);

    // --- Page chrome -------------------------------------------------------
    let header = libadwaita::HeaderBar::new();
    let toolbar = libadwaita::ToolbarView::builder().build();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&prefs));

    let page = libadwaita::NavigationPage::builder()
        .title("Backup")
        .child(&toolbar)
        .build();

    // Recompute the counters + refresh the folder subtitle every time this page
    // becomes the visible top of the nav — this fires on first open AND each
    // time the folder-select page is popped back off, so a changed selection is
    // always reflected without the select page reaching back into this one.
    //
    // (`AdwNavigationPage::shown` fires when the page is fully revealed; the
    // matching `hidden` fires when it is COVERED as well as when popped, so we
    // deliberately drive the refresh from `shown`, not `hidden`.)
    page.connect_shown(clone!(
        #[strong]
        ui,
        #[strong]
        folders_row,
        #[strong]
        total_label,
        #[strong]
        backed_label,
        #[strong]
        remaining_label,
        #[strong]
        status_group,
        #[strong]
        status_row,
        #[strong]
        status_progress,
        #[strong]
        items_list,
        #[strong]
        tick_started,
        #[strong]
        thumb_cache,
        move |_| {
            folders_row.set_subtitle(&selected_folders_subtitle(&ui));
            spawn_counter_compute(
                ui.clone(),
                total_label.clone(),
                backed_label.clone(),
                remaining_label.clone(),
            );
            if !tick_started.replace(true) {
                start_status_tick(
                    ui.clone(),
                    status_group.clone(),
                    status_row.clone(),
                    status_progress.clone(),
                    items_list.clone(),
                    thumb_cache.clone(),
                );
            } else {
                // Timer already running; just repaint immediately.
                refresh_backup_status(
                    &ui,
                    &status_group,
                    &status_row,
                    &status_progress,
                    &items_list,
                    &thumb_cache,
                );
            }
        }
    ));

    // Wire the "Select folders" drill-in (§3). It only pushes the page; the
    // `shown` handler above handles the refresh when the user returns.
    select_button.connect_clicked(clone!(
        #[strong]
        ui,
        move |_| present_folder_select(ui.clone())
    ));

    // Wire "Back up now": enqueue every not-backed-up file (§5).
    backup_now_button.connect_clicked(clone!(
        #[strong]
        ui,
        move |btn| {
            start_backup_now(ui.clone(), btn.clone());
        }
    ));

    page
}

/// Build a counter row: a title/subtitle `ActionRow` with a bold value suffix
/// label. Returns the row plus its value label so the async compute can fill it.
fn counter_row(title: &str, subtitle: &str) -> (libadwaita::ActionRow, gtk::Label) {
    let value = gtk::Label::builder()
        .label("…")
        .css_classes(["title-2"])
        .valign(gtk::Align::Center)
        .build();
    let row = libadwaita::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .subtitle_lines(2)
        .build();
    row.add_suffix(&value);
    (row, value)
}

/// Build a subtitle listing the selected folders' basenames, e.g.
/// "Selected: Camera, Screenshots" (matches IMG_0121).
fn selected_folders_subtitle(ui: &LibraryWindowUi) -> String {
    let entries = ui.ctx.live_watch_paths.lock();
    if entries.is_empty() {
        return "No folders selected".to_string();
    }
    let names: Vec<String> = entries
        .iter()
        .map(|entry| basename(entry.path()))
        .collect();
    format!("Selected: {}", names.join(", "))
}

/// Extract a display basename from a path string.
fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Persist the "Enable backup" toggle to config (§1).
///
/// The auto-backup scheduler itself is a process-lifetime GLib timeout spawned
/// once at bootstrap (see [`spawn_backup_scheduler`]); it re-checks
/// `backup_enabled` on every tick and no-ops while off. Toggling the switch on
/// here also kicks a single immediate backup tick so the user gets prompt
/// feedback without having to wait up to `BACKUP_TICK_SECS` for the first
/// scheduled pass. Toggling off needs nothing special — the next tick (and any
/// in-flight batch) just runs to completion and subsequent ticks no-op.
fn set_backup_enabled(ui: &LibraryWindowUi, enabled: bool) {
    let mut config = ui.ctx.config.write();
    if config.data.backup_enabled == enabled {
        return;
    }
    config.data.backup_enabled = enabled;
    if !config.save() {
        log::error!("Failed to save config after toggling backup_enabled");
    }
    drop(config);

    // Prompt feedback when the user opts in: run one backup pass right now.
    if enabled {
        trigger_backup_tick(ui);
    }
}

// ---------------------------------------------------------------------------
// §4  Counters (async)
// ---------------------------------------------------------------------------

/// Enumerate the selected folders, resolve each checksum, ask the server which
/// checksums it already has, and fill in Total / Backed up / Remaining.
///
/// Runs on the GLib main context via `spawn_local`; the actual hashing is
/// offloaded to `spawn_blocking`. Before each UI write we check that the labels
/// are still rooted (`root().is_some()`) — a popped-and-dropped page loses its
/// root, so a stale in-flight task simply becomes a no-op instead of touching a
/// dead widget.
fn spawn_counter_compute(
    ui: Rc<LibraryWindowUi>,
    total_label: gtk::Label,
    backed_label: gtk::Label,
    remaining_label: gtk::Label,
) {
    total_label.set_text("…");
    backed_label.set_text("…");
    remaining_label.set_text("…");

    glib::MainContext::default().spawn_local(async move {
        let ctx = ui.ctx.clone();

        // 1. Enumerate local files across the selected watch paths. `Total` is
        //    the number of enumerated media files: a file that later fails to
        //    hash still counts toward Total and falls into `Remaining` (honest
        //    — we can't prove it is on the server), rather than vanishing.
        let locals = crate::library::local_source::enumerate_local(ctx.clone()).await;
        if total_label.root().is_none() {
            return;
        }
        let paths: Vec<PathBuf> = locals.into_iter().map(|a| a.path).collect();
        let total = paths.len();

        // 2. Resolve each file's checksum, keeping the (path, checksum) pair so
        //    we can compute remaining per-file (mirroring what start_backup_now
        //    uses for its enqueue filter). Files that fail to hash are counted
        //    as remaining — we can't prove they are on the server.
        let sync_index = ctx.sync_index.clone();
        let pairs: Vec<(PathBuf, String)> = tokio::task::spawn_blocking(move || {
            let mut out = Vec::with_capacity(paths.len());
            for path in paths {
                let checksum = sync_index.fresh_checksum(&path).or_else(|| {
                    crate::monitor::compute_sha1_chunked(&path.to_string_lossy()).ok()
                });
                if let Some(checksum) = checksum {
                    out.push((path, checksum));
                }
            }
            out
        })
        .await
        .unwrap_or_default();
        if total_label.root().is_none() {
            return;
        }
        // Files that failed to hash: count them as remaining.
        let unhashed = total.saturating_sub(pairs.len());

        // Dedup checksums so a file present in two folders isn't double-counted
        // when asking the server.
        let unique: Vec<String> = {
            let set: std::collections::HashSet<String> =
                pairs.iter().map(|(_, c)| c.clone()).collect();
            set.into_iter().collect()
        };

        // 3. Ask the server which of those checksums it already has.
        let server = ctx.api_client.bulk_existing_asset_ids(&unique).await;
        if total_label.root().is_none() {
            return;
        }
        // Remaining = files whose checksum is not on the server + files that
        // couldn't be hashed. This mirrors the enqueue filter in start_backup_now
        // so the counter matches what "Back up now" will actually upload.
        let remaining = pairs
            .iter()
            .filter(|(_, checksum)| !server.contains_key(checksum))
            .count()
            + unhashed;
        let backed_up = total.saturating_sub(remaining);

        total_label.set_text(&total.to_string());
        backed_label.set_text(&backed_up.to_string());
        remaining_label.set_text(&remaining.to_string());
    });
}

// ---------------------------------------------------------------------------
// Live upload status (mirrors the header backup icon)
// ---------------------------------------------------------------------------

/// Start a 250 ms tick that repaints the live upload-status group from the
/// shared `TransferSnapshot`. Self-cancels once the page is dropped: `status_group`
/// loses its `root()` when the owning page is popped-and-dropped, so a stale tick
/// becomes a no-op and returns `ControlFlow::Break`.
fn start_status_tick(
    ui: Rc<LibraryWindowUi>,
    status_group: libadwaita::PreferencesGroup,
    status_row: libadwaita::ActionRow,
    status_progress: gtk::ProgressBar,
    items_list: gtk::ListBox,
    thumb_cache: ThumbCache,
) {
    // Paint once immediately so the section is correct on first reveal.
    refresh_backup_status(
        &ui,
        &status_group,
        &status_row,
        &status_progress,
        &items_list,
        &thumb_cache,
    );
    glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
        if status_group.root().is_none() {
            return glib::ControlFlow::Break;
        }
        refresh_backup_status(
            &ui,
            &status_group,
            &status_row,
            &status_progress,
            &items_list,
            &thumb_cache,
        );
        glib::ControlFlow::Continue
    });
}

/// Repaint the live upload-status group from the current transfer snapshot.
///
/// This is the "Cargar detalles" surface: a summary header ("Uploading N") over
/// a per-item list, one row per active upload — thumbnail, filename, per-item
/// progress bar, and a `bytes / total (pct%)` byte read-out — all driven by
/// `TransferSnapshot`'s per-item `active_item_bytes` / `active_item_totals`.
fn refresh_backup_status(
    ui: &LibraryWindowUi,
    status_group: &libadwaita::PreferencesGroup,
    status_row: &libadwaita::ActionRow,
    status_progress: &gtk::ProgressBar,
    items_list: &gtk::ListBox,
    thumb_cache: &ThumbCache,
) {
    use crate::state_manager::TransferDirection;

    let transfer = ui.ctx.state.lock().transfer.clone();
    let uploading =
        transfer.active && matches!(transfer.direction, TransferDirection::Upload);
    if !uploading {
        status_group.set_visible(false);
        return;
    }
    status_group.set_visible(true);

    let n = transfer.active_uploads;
    status_row.set_title(&format!(
        "Uploading {n} item{}",
        if n == 1 { "" } else { "s" }
    ));
    status_row.set_subtitle(
        transfer
            .active_item_label
            .as_deref()
            .unwrap_or("queued asset"),
    );

    match transfer.total_bytes {
        Some(total) if total > 0 => {
            status_progress.set_fraction(
                (transfer.current_bytes as f64 / total as f64).clamp(0.0, 1.0),
            );
        }
        _ => status_progress.pulse(),
    }

    // Per-item list: one row per active item with a thumbnail, filename,
    // progress bar, and byte/percent read-out. Rebuilt each tick — the active
    // set is small (a few parallel workers), so clearing and re-appending is
    // cheap and keeps the code simple. Thumbnails are cached across ticks in
    // `thumb_cache` so we don't re-decode the same file every 250 ms.
    while let Some(child) = items_list.first_child() {
        items_list.remove(&child);
    }
    let mut items: Vec<(&String, u64)> = transfer
        .active_item_bytes
        .iter()
        .map(|(id, bytes)| (id, *bytes))
        .collect();
    items.sort_by(|a, b| a.0.cmp(b.0));
    for (id, bytes) in items {
        let total = transfer.active_item_totals.get(id).copied().unwrap_or(0);
        let bar = gtk::ProgressBar::builder()
            .valign(gtk::Align::Center)
            .hexpand(true)
            .width_request(96)
            .build();
        let pct = if total > 0 {
            let f = (bytes as f64 / total as f64).clamp(0.0, 1.0);
            bar.set_fraction(f);
            (f * 100.0).round() as u32
        } else {
            bar.pulse();
            0
        };

        let row = libadwaita::ActionRow::builder()
            .title(&item_display_name(id))
            .title_lines(1)
            .subtitle(&byte_progress_text(bytes, total, pct))
            .build();
        row.add_prefix(&item_thumbnail(ui, id, thumb_cache));
        row.add_suffix(&bar);
        items_list.append(&row);
    }
    items_list.set_visible(items_list.first_child().is_some());
}

/// Format the per-item byte read-out, e.g. "1.2 MB / 3.4 MB · 35%". Falls back
/// to just the transferred bytes when the total isn't known yet.
fn byte_progress_text(bytes: u64, total: u64, pct: u32) -> String {
    if total > 0 {
        format!(
            "{} / {} · {pct}%",
            human_bytes(bytes),
            human_bytes(total)
        )
    } else {
        human_bytes(bytes)
    }
}

/// Compact human-readable byte size (binary units), e.g. "1.2 MB".
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Build a small rounded thumbnail widget for an upload-details row.
///
/// The transfer item key is the file's absolute path, so we can load a local
/// thumbnail directly. A cached texture is applied synchronously; otherwise a
/// generic image icon is shown and an async decode is kicked off that fills the
/// picture in (and caches it) when it resolves. Guarded against a dropped page
/// by checking the picture is still rooted before applying the late texture.
fn item_thumbnail(ui: &LibraryWindowUi, path: &str, thumb_cache: &ThumbCache) -> gtk::Widget {
    let thumb = gtk::Overlay::builder()
        .overflow(gtk::Overflow::Hidden)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .css_classes(vec!["mimick-explore-tile".to_string()])
        .build();
    let spacer = gtk::Box::builder().build();
    spacer.set_size_request(40, 40);
    let picture = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Cover)
        .build();
    thumb.set_child(Some(&spacer));

    if let Some(texture) = thumb_cache.borrow().get(path).cloned() {
        picture.set_paintable(Some(&texture));
        thumb.add_overlay(&picture);
        return thumb.upcast();
    }

    // No cached thumbnail yet: show a generic placeholder icon and load async.
    let placeholder = gtk::Image::from_icon_name("image-x-generic-symbolic");
    placeholder.set_pixel_size(20);
    thumb.add_overlay(&placeholder);
    thumb.add_overlay(&picture);

    let file_path = std::path::PathBuf::from(path);
    if file_path.is_file() {
        let ctx = ui.ctx.clone();
        let key = path.to_string();
        let cache = thumb_cache.clone();
        glib::MainContext::default().spawn_local(clone!(
            #[strong]
            picture,
            #[strong]
            placeholder,
            async move {
                let result = ctx
                    .thumbnail_cache
                    .load_local_thumbnail_cancellable(&key, &file_path, || false)
                    .await;
                if let Ok(texture) = result {
                    cache.borrow_mut().insert(key, texture.clone());
                    // The row may have been rebuilt on a later tick (which reads
                    // the now-warm cache) — only touch this picture if it's still
                    // in the tree.
                    if picture.root().is_some() {
                        picture.set_paintable(Some(&texture));
                        placeholder.set_visible(false);
                    }
                }
            }
        ));
    }

    thumb.upcast()
}

/// Best-effort display name for a per-item transfer key (a path or id).
fn item_display_name(id: &str) -> String {
    Path::new(id)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| id.to_string())
}

// ---------------------------------------------------------------------------
// §5  "Back up now" — reuse the existing upload engine
// ---------------------------------------------------------------------------

/// Enqueue every not-backed-up file across the selected folders, sharing the
/// same enumerate → hash → server-check → filter → enqueue pipeline as the
/// manual "Back up now" button. This is the single shared core used by both the
/// button ([`start_backup_now`]) and the auto-backup scheduler
/// ([`spawn_backup_scheduler`]/[`trigger_backup_tick`]), so the two paths can
/// never double-enqueue and the foreground UI stays truthful.
///
/// Returns `Some(count)` with the number of files handed to the queue (and
/// immediately kicked off the enqueue), or `None` when there was nothing to
/// back up (or enumeration/hashing yielded nothing).
///
/// `on_complete(queued, skipped)` fires on the GLib main context once the
/// underlying enqueue task has hashed and queued every file. Both callers use
/// it to release the [`AppContext::backup_in_progress`] guard; the manual
/// button additionally uses it to restore its label. The post-batch
/// [`crate::app_context::refresh_server_checksums`] is invoked from inside this
/// helper's completion wrapper so BOTH paths refresh the shared
/// `server_checksums` set — that is the dedup guarantee that keeps the badge
/// classifier truthful and prevents the next tick from re-uploading.
async fn enqueue_unbacked_files<F>(
    ctx: Arc<AppContext>,
    on_complete: F,
) -> Option<usize>
where
    F: FnOnce(usize, usize) + 'static,
{
    let locals = crate::library::local_source::enumerate_local(ctx.clone()).await;
    let paths: Vec<PathBuf> = locals.into_iter().map(|a| a.path).collect();

    // Resolve checksums so we can skip files already on the server. Hashing is
    // off-thread (CPU-heavy); `fresh_checksum` is a free cache hit when the
    // file's size+mtime are unchanged, otherwise we hash on the blocking pool.
    let sync_index = ctx.sync_index.clone();
    let paths_for_hash = paths.clone();
    let pairs: Vec<(PathBuf, String)> = tokio::task::spawn_blocking(move || {
        let mut out = Vec::with_capacity(paths_for_hash.len());
        for path in paths_for_hash {
            let checksum = sync_index.fresh_checksum(&path).or_else(|| {
                crate::monitor::compute_sha1_chunked(&path.to_string_lossy()).ok()
            });
            if let Some(checksum) = checksum {
                out.push((path, checksum));
            }
        }
        out
    })
    .await
    .unwrap_or_default();

    let all_checksums: Vec<String> = pairs.iter().map(|(_, c)| c.clone()).collect();
    let server = ctx.api_client.bulk_existing_asset_ids(&all_checksums).await;

    let to_upload: Vec<PathBuf> = pairs
        .into_iter()
        .filter(|(_, checksum)| !server.contains_key(checksum))
        .map(|(path, _)| path)
        .collect();

    let count = to_upload.len();
    if count == 0 {
        return None;
    }

    // Upload straight to the library (no album). The wrapped callback runs after
    // every file has been hashed + enqueued, and is what releases the guard +
    // refreshes the shared server-checksum set so both paths benefit.
    let ctx_for_refresh = ctx.clone();
    crate::library::upload_picker::spawn_enqueue_with_callback(
        ctx,
        None,
        to_upload,
        move |queued, skipped| {
            log::info!("Backup: queued {}/{} file(s), skipped {skipped}", queued, count);
            // Refresh the authoritative server-checksum set so the badge
            // classifier (and the next scheduler tick) see what just landed
            // and don't re-upload it. Spawned on the main context because the
            // callback itself is synchronous.
            let ctx_refresh = ctx_for_refresh.clone();
            glib::MainContext::default().spawn_local(async move {
                crate::app_context::refresh_server_checksums(ctx_refresh).await;
            });
            on_complete(queued, skipped);
        },
    );

    Some(count)
}

/// Manually trigger "Back up now" from the button (§5).
///
/// UX is identical to before: "Preparing…" while enumerate/hashing runs,
/// "Backing up N…" once the batch is handed to the queue, "All backed up" when
/// nothing was pending, and "Back up now" once the enqueue completes.
///
/// Respects the shared [`AppContext::backup_in_progress`] guard: if a backup
/// (manual or scheduled) is already in flight, the tap is a no-op rather than
/// queueing a second batch.
fn start_backup_now(ui: Rc<LibraryWindowUi>, button: gtk::Button) {
    // Re-entrancy guard: claim synchronously so two rapid taps (or a tap while
    // a scheduler tick is running) can't enqueue the same set twice. Released
    // either here (nothing to enqueue) or in the completion callback.
    if ui.ctx.backup_in_progress.swap(true, Ordering::SeqCst) {
        return;
    }
    button.set_sensitive(false);
    button.set_label("Preparing…");

    glib::MainContext::default().spawn_local(clone!(
        #[strong]
        ui,
        #[strong]
        button,
        async move {
            let ctx = ui.ctx.clone();
            let count = enqueue_unbacked_files(
                ctx.clone(),
                clone!(
                    #[strong]
                    button,
                    #[strong]
                    ui,
                    move |queued, _skipped| {
                        log::info!("Back up now: enqueued {queued} file(s)");
                        button.set_label("Back up now");
                        button.set_sensitive(true);
                        ui.ctx.backup_in_progress.store(false, Ordering::SeqCst);
                    }
                ),
            )
            .await;

            match count {
                None => {
                    button.set_label("All backed up");
                    button.set_sensitive(true);
                    ui.ctx.backup_in_progress.store(false, Ordering::SeqCst);
                }
                Some(n) => {
                    // Immediate feedback while the enqueue task hashes files.
                    button.set_label(&format!("Backing up {n}…"));
                }
            }
        }
    ));
}

/// Run a single auto-backup pass if `backup_enabled` is on and no batch is
/// already running. Shared by [`spawn_backup_scheduler`]'s periodic ticks and
/// by [`set_backup_enabled`]'s immediate-on-enable kick. Operates purely on the
/// shared [`AppContext`] (no widget touches), so it is safe to call from either
/// the timeout closure or a switch notify handler.
fn trigger_backup_tick(ui: &LibraryWindowUi) {
    // Re-check the flag here too: the user may have disabled backup between the
    // scheduler's spawn and this call (or this is the on-enable kick and the
    // flag was just flipped off by a racing toggle).
    if !ui.ctx.config.read().data.backup_enabled {
        return;
    }
    // Claim the guard synchronously; if a manual "Back up now" or another tick
    // is mid-flight, this tick silently no-ops.
    if ui.ctx.backup_in_progress.swap(true, Ordering::SeqCst) {
        return;
    }

    let ctx = ui.ctx.clone();
    glib::MainContext::default().spawn_local(async move {
        let ctx_for_cb = ctx.clone();
        let count = enqueue_unbacked_files(
            ctx.clone(),
            move |queued, skipped| {
                log::debug!("Auto-backup tick: queued {queued}, skipped {skipped}");
                ctx_for_cb.backup_in_progress.store(false, Ordering::SeqCst);
            },
        )
        .await;
        // Nothing to enqueue this pass: release the guard immediately so the
        // next tick (or a manual button press) isn't blocked.
        if count.is_none() {
            ctx.backup_in_progress.store(false, Ordering::SeqCst);
        }
    });
}

/// Spawn the in-process auto-backup scheduler: a periodic GLib timeout on the
/// default main context that, every [`BACKUP_TICK_SECS`] seconds while the app
/// is alive, enqueues every not-backed-up file across the backup watch folders
/// via [`enqueue_unbacked_files`] (the same path "Back up now" uses).
///
/// The timeout persists for the application lifetime and dies with the process
/// when the window close → `app.quit()` path (already wired by the orchestrator)
/// fires — there is intentionally NO `app.hold()` here, so closing the window
/// kills the scheduler. A future separate out-of-process background-sync daemon
/// is deferred; this is the in-app, while-open auto-backup.
///
/// Each tick no-ops while `config.backup_enabled` is off or while
/// [`AppContext::backup_in_progress`] is already claimed, and shares the same
/// `server_checksums` set + `QueueManager` pipeline as the manual button, so the
/// foreground UI and the scheduler never double-upload and the badge classifier
/// stays truthful.
pub fn spawn_backup_scheduler(ui: Rc<LibraryWindowUi>) {
    glib::timeout_add_local(std::time::Duration::from_secs(BACKUP_TICK_SECS as u64), clone!(
        #[strong]
        ui,
        move || {
            trigger_backup_tick(&ui);
            // Persist for the app lifetime; close→quit kills the loop.
            glib::ControlFlow::Continue
        }
    ));
}

// Note: the `ctx.clone()` sites above rely on `ctx: Arc<AppContext>`; the type
// is inferred from `AppContext` handles and needs no explicit `Arc` import.

// ---------------------------------------------------------------------------
// §3  Folder-select page (IMG_0123)
// ---------------------------------------------------------------------------

/// Push the "Select folders" page. Candidate folders are the immediate
/// subdirectories of `~/Pictures/` plus any folder already in
/// `config.watch_paths`. Toggling a row's check adds/removes a
/// `WatchPathEntry::Simple(path)` and persists it. When this page is popped the
/// backup home page's `shown` handler recomputes its counters + subtitle, so
/// this page needs no reference back into it.
fn present_folder_select(ui: Rc<LibraryWindowUi>) {
    let candidates = candidate_folders(&ui);

    let list_box = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    // Collect the per-row check buttons so Select all / Deselect all can drive
    // them (each toggle handler does the persistence).
    let checks: Rc<std::cell::RefCell<Vec<gtk::CheckButton>>> =
        Rc::new(std::cell::RefCell::new(Vec::new()));

    for path in &candidates {
        let selected = is_selected(&ui, path);
        let count = quick_media_count(path);

        let check = gtk::CheckButton::builder()
            .active(selected)
            .valign(gtk::Align::Center)
            .build();

        let row = libadwaita::ActionRow::builder()
            .title(&basename(&path.to_string_lossy()))
            .subtitle(&format!("{count} items"))
            .build();
        row.add_suffix(&check);
        row.set_activatable_widget(Some(&check));

        // Toggling a check adds/removes the folder from watch_paths + persists.
        let path_owned = path.clone();
        check.connect_toggled(clone!(
            #[strong]
            ui,
            move |btn| {
                set_folder_selected(&ui, &path_owned, btn.is_active());
            }
        ));

        checks.borrow_mut().push(check);
        list_box.append(&row);
    }

    // --- Select all / Deselect all ----------------------------------------
    let select_all = gtk::Button::builder()
        .label("Select all")
        .hexpand(true)
        .css_classes(["mimick-pressable"])
        .build();
    let deselect_all = gtk::Button::builder()
        .label("Deselect all")
        .hexpand(true)
        .css_classes(["mimick-pressable"])
        .build();
    select_all.connect_clicked(clone!(
        #[strong]
        checks,
        move |_| {
            for check in checks.borrow().iter() {
                check.set_active(true);
            }
        }
    ));
    deselect_all.connect_clicked(clone!(
        #[strong]
        checks,
        move |_| {
            for check in checks.borrow().iter() {
                check.set_active(false);
            }
        }
    ));
    let button_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .homogeneous(true)
        .build();
    button_row.append(&select_all);
    button_row.append(&deselect_all);

    // --- Page body ---------------------------------------------------------
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .spacing(4)
        .build();
    let hint = gtk::Label::builder()
        .label("Choose the folders to include in the backup")
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label", "caption"])
        .margin_bottom(4)
        .build();
    body.append(&hint);
    body.append(&list_box);

    let scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hexpand(true)
        .child(&body)
        .build();

    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    outer.append(&button_row);
    outer.append(&scroller);

    let header = libadwaita::HeaderBar::new();
    let toolbar = libadwaita::ToolbarView::builder().build();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&outer));

    let page = libadwaita::NavigationPage::builder()
        .title("Select folders")
        .child(&toolbar)
        .build();

    ui.nav.push(&page);
}

/// Collect candidate backup folders: immediate subdirectories of `~/Pictures/`
/// plus any folder already configured in `watch_paths` (so a manually-added
/// folder outside `~/Pictures` still appears and stays toggleable). Deduped and
/// path-sorted.
fn candidate_folders(ui: &LibraryWindowUi) -> Vec<PathBuf> {
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut out: Vec<PathBuf> = Vec::new();

    if let Some(pictures) = dirs::picture_dir() {
        if let Ok(read_dir) = std::fs::read_dir(&pictures) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.is_dir() && seen.insert(path.clone()) {
                    out.push(path);
                }
            }
        }
    }

    // Any already-selected folder (even outside ~/Pictures) must stay listed.
    for entry in ui.ctx.live_watch_paths.lock().iter() {
        let path = PathBuf::from(entry.path());
        if seen.insert(path.clone()) {
            out.push(path);
        }
    }

    out.sort();
    out
}

/// Whether `path` is currently one of the selected backup folders.
fn is_selected(ui: &LibraryWindowUi, path: &Path) -> bool {
    let target = path.to_string_lossy();
    let target = target.as_ref();
    ui.ctx
        .live_watch_paths
        .lock()
        .iter()
        .any(|entry| entry.path() == target)
}

/// Add or remove `path` from `watch_paths` and persist, mirroring the
/// link/unlink persistence idiom in `album_link.rs`. Idempotent.
fn set_folder_selected(ui: &LibraryWindowUi, path: &Path, selected: bool) {
    let path_string = path.to_string_lossy().to_string();
    {
        let mut config = ui.ctx.config.write();
        let already = config
            .data
            .watch_paths
            .iter()
            .any(|entry| entry.path() == path_string.as_str());
        if selected {
            if already {
                return;
            }
            config
                .data
                .watch_paths
                .push(WatchPathEntry::Simple(path_string.clone()));
        } else {
            if !already {
                return;
            }
            config
                .data
                .watch_paths
                .retain(|entry| entry.path() != path_string.as_str());
        }
        if !config.save() {
            log::error!("Failed to save config after backup folder toggle");
            return;
        }
        *ui.ctx.live_watch_paths.lock() = config.data.watch_paths.clone();
    }
    // Nudge the sync engine so the selection takes effect promptly.
    let _ = ui.ctx.sync_now_tx.send(());
}

/// Cheap count of image/video files directly inside `dir` (non-recursive,
/// no hashing) for the row subtitle. Matches the enumeration's supported-media
/// gate without walking the whole tree.
fn quick_media_count(dir: &Path) -> usize {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return 0;
    };
    read_dir
        .flatten()
        .filter(|entry| {
            let path = entry.path();
            path.is_file() && crate::monitor::is_supported_media_path(&path)
        })
        .count()
}
