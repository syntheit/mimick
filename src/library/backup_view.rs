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

use glib::clone;
use gtk::prelude::*;
use libadwaita::prelude::*;

use super::LibraryWindowUi;
use crate::config::WatchPathEntry;

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
                );
            } else {
                // Timer already running; just repaint immediately.
                refresh_backup_status(
                    &ui,
                    &status_group,
                    &status_row,
                    &status_progress,
                    &items_list,
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
fn set_backup_enabled(ui: &LibraryWindowUi, enabled: bool) {
    let mut config = ui.ctx.config.write();
    if config.data.backup_enabled == enabled {
        return;
    }
    config.data.backup_enabled = enabled;
    if !config.save() {
        log::error!("Failed to save config after toggling backup_enabled");
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
) {
    // Paint once immediately so the section is correct on first reveal.
    refresh_backup_status(&ui, &status_group, &status_row, &status_progress, &items_list);
    glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
        if status_group.root().is_none() {
            return glib::ControlFlow::Break;
        }
        refresh_backup_status(&ui, &status_group, &status_row, &status_progress, &items_list);
        glib::ControlFlow::Continue
    });
}

/// Repaint the live upload-status group from the current transfer snapshot.
fn refresh_backup_status(
    ui: &LibraryWindowUi,
    status_group: &libadwaita::PreferencesGroup,
    status_row: &libadwaita::ActionRow,
    status_progress: &gtk::ProgressBar,
    items_list: &gtk::ListBox,
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

    // Per-item list: one row per active item with its own progress bar. Rebuilt
    // each tick — the active set is small (a few parallel workers), so clearing
    // and re-appending is cheap and keeps the code simple.
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
        if total > 0 {
            bar.set_fraction((bytes as f64 / total as f64).clamp(0.0, 1.0));
        } else {
            bar.pulse();
        }
        let row = libadwaita::ActionRow::builder()
            .title(&item_display_name(id))
            .title_lines(1)
            .build();
        row.add_suffix(&bar);
        items_list.append(&row);
    }
    items_list.set_visible(items_list.first_child().is_some());
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

/// Enqueue an upload for every not-backed-up file across the selected folders.
///
/// Reuses `upload_picker::spawn_enqueue_with_callback` (straight to the library,
/// `album = None`) so the existing dedup (409) + queue + transfer-bar plumbing
/// handles everything. Files already on the server are filtered out up front via
/// `bulk_existing_asset_ids`, and the queue's own dedup catches any that slip
/// through.
fn start_backup_now(ui: Rc<LibraryWindowUi>, button: gtk::Button) {
    button.set_sensitive(false);
    button.set_label("Preparing…");

    glib::MainContext::default().spawn_local(async move {
        let ctx = ui.ctx.clone();

        let locals = crate::library::local_source::enumerate_local(ctx.clone()).await;
        let paths: Vec<PathBuf> = locals.into_iter().map(|a| a.path).collect();

        // Resolve checksums so we can skip files already on the server.
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
            button.set_label("All backed up");
            button.set_sensitive(true);
            return;
        }

        // Upload straight to the library (no album). The callback runs after
        // every file has been hashed + enqueued.
        crate::library::upload_picker::spawn_enqueue_with_callback(
            ctx,
            None,
            to_upload,
            clone!(
                #[strong]
                button,
                move |queued, _skipped| {
                    log::info!("Backup now: queued {}/{} file(s)", queued, count);
                    button.set_label("Back up now");
                    button.set_sensitive(true);
                }
            ),
        );

        // Immediate feedback while the enqueue task hashes files.
        button.set_label(&format!("Backing up {count}…"));
    });
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
