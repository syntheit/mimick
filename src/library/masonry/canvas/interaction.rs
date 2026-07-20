use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::ObjectSubclassIsExt;

use super::{
    AssetContextMenuHandler, GridLayout, GridQuality, LibraryAssetModel, MasonryCanvas,
    ThumbnailCache, imp, item_at_x, row_at_y,
};
use crate::app_context::AppContext;
use crate::library::asset_object::AssetObject;

impl Default for MasonryCanvas {
    fn default() -> Self {
        gtk::glib::Object::new()
    }
}

impl MasonryCanvas {
    pub fn new(
        cache: Arc<ThumbnailCache>,
        model: LibraryAssetModel,
        selection: gtk::MultiSelection,
    ) -> Self {
        let canvas: Self = gtk::glib::Object::new();
        let imp = canvas.imp();
        let _ = imp.cache.set(cache);
        let _ = imp.model.set(model.clone());
        let _ = imp.selection.set(selection.clone());

        connect_model_changes(&canvas, &model);
        connect_selection_changes(&canvas, &selection);
        canvas.install_gestures();
        canvas
    }

    pub fn set_narrow(&self, narrow: bool) {
        let imp = self.imp();
        if imp.narrow.replace(narrow) != narrow {
            imp.invalidate_layout();
            self.queue_draw();
        }
        // On the phone (narrow) detach the drag-export controller so touch drags
        // are handed to the ScrolledWindow for scrolling instead of starting a
        // DnD export. Drag-export stays available on the desktop.
        self.set_drag_export_enabled(!narrow);
    }

    /// Attach or detach the drag-export `DragSource` controller. See the field
    /// docs on `imp::MasonryCanvas::drag_source`. No-op until `install_drag_source`
    /// has run (the controller is created there).
    pub fn set_drag_export_enabled(&self, on: bool) {
        let imp = self.imp();
        if imp.drag_source_active.get() == on {
            return;
        }
        let Some(ds) = imp.drag_source.borrow().clone() else {
            return;
        };
        if on {
            self.add_controller(ds);
        } else {
            self.remove_controller(&ds);
        }
        imp.drag_source_active.set(on);
    }

    pub fn set_select_mode(&self, on: bool) {
        self.imp().select_mode.set(on);
    }

    pub fn set_quality(&self, quality: GridQuality) {
        let imp = self.imp();
        if imp.quality.replace(quality) != quality {
            self.queue_draw();
        }
    }

    pub fn set_layout_mode(&self, mode: GridLayout) {
        let imp = self.imp();
        if imp.layout_mode.replace(mode) != mode {
            imp.invalidate_layout();
            self.queue_draw();
        }
    }

    pub fn set_grid_columns(&self, cols: u32) {
        let imp = self.imp();
        let new_cols = cols.max(1);
        if imp.grid_columns.replace(new_cols) != new_cols {
            imp.invalidate_layout();
            self.queue_draw();
        }
    }

    pub fn set_activate_handler(&self, f: impl Fn(u32) + 'static) {
        *self.imp().activate_handler.borrow_mut() = Some(Rc::new(f));
    }

    pub fn set_context_menu_handler(&self, handler: AssetContextMenuHandler) {
        *self.imp().context_menu_handler.borrow_mut() = Some(handler);
    }

    pub fn set_select_mode_changer(&self, f: impl Fn(bool) + 'static) {
        *self.imp().select_mode_changer.borrow_mut() = Some(Rc::new(f));
    }

    fn hit_test(&self, x: f64, y: f64) -> Option<u32> {
        let rows = self.imp().rows.borrow();
        let r = row_at_y(&rows, y as f32)?;
        let row = &rows[r];
        item_at_x(row, x as f32).map(|it| it.asset_index)
    }

    fn handle_primary_click(&self, gesture: &gtk::GestureClick, x: f64, y: f64) {
        let Some(pos) = self.hit_test(x, y) else {
            return;
        };
        let imp = self.imp();
        let mods = gesture.current_event_state();
        let ctrl = mods.contains(gtk::gdk::ModifierType::CONTROL_MASK);
        let shift = mods.contains(gtk::gdk::ModifierType::SHIFT_MASK);
        let Some(sel) = imp.selection.get() else {
            return;
        };

        if shift {
            // Shift+Click: select contiguous range [anchor..pos].
            let anchor = imp.last_selected.get().unwrap_or(pos);
            let lo = anchor.min(pos);
            let hi = anchor.max(pos);
            // Shift only: replace selection with range. Ctrl+Shift: union.
            if !ctrl {
                sel.unselect_all();
            }
            select_range(sel, lo, hi);
            enable_select_mode_if_needed(imp, pos);
            // Preserve anchor so repeated Shift+Clicks extend from the same origin.
        } else if ctrl {
            toggle_selection(sel, pos);
            enable_select_mode_if_needed(imp, pos);
            imp.last_selected.set(Some(pos));
        } else if imp.select_mode.get() {
            toggle_selection(sel, pos);
            imp.last_selected.set(Some(pos));
        } else if let Some(handler) = imp.activate_handler.borrow().clone() {
            (*handler)(pos);
        }
    }

    fn handle_secondary_click(&self, x: f64, y: f64) {
        let Some(pos) = self.hit_test(x, y) else {
            return;
        };
        let imp = self.imp();
        if let Some(handler_cell) = imp.context_menu_handler.borrow().clone()
            && let Some(cb) = handler_cell.borrow().as_ref()
        {
            (cb)(pos, x, y);
        }
    }

    fn install_gestures(&self) {
        self.add_controller(primary_click_controller(self));
        self.add_controller(secondary_click_controller(self));
        // Touch multi-select: long-press to enter select mode + anchor a tile,
        // then a drag from that same press range-selects with edge auto-scroll.
        // The long-press and drag gestures share a group so only one wins the
        // sequence; the drag defers to normal scrolling unless a long-press has
        // armed `range_drag_active`.
        let long_press = long_press_controller(self);
        let range_drag = range_drag_controller(self);
        long_press.group_with(&range_drag);
        self.add_controller(long_press);
        self.add_controller(range_drag);
    }

    /// Enter select mode (if needed) and select the tile under (x, y), arming a
    /// range-select drag anchored there. Called from the long-press handler.
    fn begin_range_select(&self, x: f64, y: f64) {
        let Some(pos) = self.hit_test(x, y) else {
            return;
        };
        let imp = self.imp();
        let Some(sel) = imp.selection.get() else {
            return;
        };
        enable_select_mode_if_needed(imp, pos);
        // Anchor the range at the long-pressed tile and select it.
        sel.select_item(pos, false);
        imp.last_selected.set(Some(pos));
        imp.range_anchor.set(Some(pos));
        imp.range_drag_active.set(true);
        imp.range_pointer_y.set(y);
        imp.range_pointer_x.set(x);
    }

    /// Update the live range selection to span [anchor..=tile-under-pointer].
    /// Coordinates are widget-space; the tile is resolved against the current
    /// scroll offset so scrolled-in rows are covered.
    fn update_range_select(&self, x: f64, y: f64) {
        let imp = self.imp();
        if !imp.range_drag_active.get() {
            return;
        }
        imp.range_pointer_y.set(y);
        imp.range_pointer_x.set(x);
        let Some(anchor) = imp.range_anchor.get() else {
            return;
        };
        let Some(pos) = self.hit_test_clamped(x, y) else {
            return;
        };
        let Some(sel) = imp.selection.get() else {
            return;
        };
        let lo = anchor.min(pos);
        let hi = anchor.max(pos);
        // Replace with the fresh anchor→pointer range so back-tracking the finger
        // deselects tiles overshot earlier (iOS/Google-Photos behaviour).
        sel.unselect_all();
        sel.select_range(lo, hi - lo + 1, false);
        imp.last_selected.set(Some(pos));
    }

    /// End the range-select drag, tearing down any auto-scroll timer.
    fn end_range_select(&self) {
        let imp = self.imp();
        imp.range_drag_active.set(false);
        imp.range_anchor.set(None);
        self.stop_autoscroll();
    }

    /// Hit-test but clamp the pointer into the widget bounds first, so a finger
    /// dragged past the top/bottom edge still resolves to the first/last row
    /// instead of missing. Empty gaps between tiles fall back to the nearest row.
    fn hit_test_clamped(&self, x: f64, y: f64) -> Option<u32> {
        let h = self.height() as f64;
        let w = self.width() as f64;
        let cy = y.clamp(1.0, (h - 1.0).max(1.0));
        let cx = x.clamp(1.0, (w - 1.0).max(1.0));
        if let Some(pos) = self.hit_test(cx, cy) {
            return Some(pos);
        }
        // Fell in a gap (e.g. a date-header row or past the last item of a
        // short row): snap to the nearest tile on the row at this y.
        self.nearest_item_on_row(cx as f32, cy as f32)
    }

    fn nearest_item_on_row(&self, x: f32, y: f32) -> Option<u32> {
        let rows = self.imp().rows.borrow();
        let r = row_at_y(&rows, y)?;
        let row = &rows[r];
        // Nearest item by horizontal centre distance.
        row.items
            .iter()
            .min_by(|a, b| {
                let da = (a.x + a.w * 0.5 - x).abs();
                let db = (b.x + b.w * 0.5 - x).abs();
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|it| it.asset_index)
    }

    /// Given the pointer y (widget-space) during a range drag, arm/adjust the
    /// edge auto-scroll. Within ~10% of the top/bottom viewport edge the grid
    /// scrolls, with speed ramping toward the very edge; outside the zone the
    /// timer is stopped.
    fn update_autoscroll(&self, y: f64) {
        let Some(adj) = self.find_scroll_adjustment() else {
            return;
        };
        let viewport_h = adj.page_size();
        if viewport_h <= 0.0 {
            return;
        }
        // Pointer y is widget-space; convert to viewport-space (relative to the
        // scrolled top) by subtracting the current scroll offset.
        let view_y = y - adj.value();
        let zone = (viewport_h * 0.12).max(48.0);
        const MAX_SPEED: f64 = 26.0; // px per tick (~60fps → ~1560 px/s)

        let velocity = if view_y < zone {
            let t = ((zone - view_y) / zone).clamp(0.0, 1.0);
            -MAX_SPEED * t
        } else if view_y > viewport_h - zone {
            let t = ((view_y - (viewport_h - zone)) / zone).clamp(0.0, 1.0);
            MAX_SPEED * t
        } else {
            0.0
        };

        self.imp().autoscroll_velocity.set(velocity);
        if velocity == 0.0 {
            self.stop_autoscroll();
        } else {
            self.start_autoscroll();
        }
    }

    fn start_autoscroll(&self) {
        let imp = self.imp();
        if imp.autoscroll_source.borrow().is_some() {
            return; // already running; velocity cell drives the speed
        }
        let weak = self.downgrade();
        let source = glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
            let Some(canvas) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let imp = canvas.imp();
            if !imp.range_drag_active.get() {
                *imp.autoscroll_source.borrow_mut() = None;
                return glib::ControlFlow::Break;
            }
            let velocity = imp.autoscroll_velocity.get();
            let Some(adj) = canvas.find_scroll_adjustment() else {
                return glib::ControlFlow::Continue;
            };
            let max = (adj.upper() - adj.page_size()).max(0.0);
            let next = (adj.value() + velocity).clamp(0.0, max);
            if (next - adj.value()).abs() > f64::EPSILON {
                adj.set_value(next);
                // Extend the selection to the row now at the leading edge we're
                // scrolling toward, so rows revealed under the stationary finger
                // get covered. `x` is held at the pointer's last column.
                let viewport_h = adj.page_size();
                let edge_y = if velocity < 0.0 {
                    next + 1.0
                } else {
                    next + viewport_h - 1.0
                };
                let px = imp.range_pointer_x.get();
                canvas.update_range_select(px, edge_y);
            }
            glib::ControlFlow::Continue
        });
        *imp.autoscroll_source.borrow_mut() = Some(source);
    }

    fn stop_autoscroll(&self) {
        let imp = self.imp();
        imp.autoscroll_velocity.set(0.0);
        if let Some(source) = imp.autoscroll_source.borrow_mut().take() {
            source.remove();
        }
    }

    /// Resolve the ScrolledWindow's vertical adjustment for auto-scroll. Reuses
    /// the cached `vadjustment` (populated lazily by the snapshot path); falls
    /// back to a parent-tree walk if the snapshot hasn't run yet.
    fn find_scroll_adjustment(&self) -> Option<gtk::Adjustment> {
        if let Some(adj) = self.imp().vadjustment.borrow().clone() {
            return Some(adj);
        }
        let mut node: Option<gtk::Widget> = self.parent();
        while let Some(w) = node {
            if let Some(sw) = w.downcast_ref::<gtk::ScrolledWindow>() {
                return Some(sw.vadjustment());
            }
            node = w.parent();
        }
        None
    }

    /// Install a `DragSource` for exporting original asset files via drag.
    ///
    /// Must be called after `new()` once the thumbnail cache is set up.
    /// The drag source resolves the original file from either the local path
    /// (for local/synced assets) or the lightbox preview cache (for remote assets
    /// that have been viewed in full resolution).
    pub fn install_drag_source(&self, ctx: Arc<AppContext>) {
        let drag_source = gtk::DragSource::new();
        drag_source.set_actions(gtk::gdk::DragAction::COPY);

        let weak = self.downgrade();
        let ctx_prepare = ctx.clone();
        drag_source.connect_prepare(move |_source, x, y| {
            let canvas = weak.upgrade()?;
            let files = collect_drag_files(&canvas, x, y, &ctx_prepare)?;
            Some(files_to_content_provider(files))
        });

        let weak2 = self.downgrade();
        drag_source.connect_drag_begin(move |source, _drag| {
            let Some(canvas) = weak2.upgrade() else {
                return;
            };
            canvas.imp().drag_active.set(true);
            let count = selected_count(&canvas);
            if count > 1 {
                source.set_icon(Some(&build_drag_badge(count)), 0, 0);
            }
        });

        let weak3 = self.downgrade();
        drag_source.connect_drag_end(move |_, _, _| {
            if let Some(canvas) = weak3.upgrade() {
                canvas.imp().drag_active.set(false);
            }
        });

        // Retain the controller so it can be detached on mobile (see set_narrow).
        *self.imp().drag_source.borrow_mut() = Some(drag_source.clone());
        self.add_controller(drag_source);
        self.imp().drag_source_active.set(true);
        // Apply the current form factor: if we're already narrow, keep it off.
        let narrow = self.imp().narrow.get();
        self.set_drag_export_enabled(!narrow);
    }
}

/// Collect gio::Files for the current drag operation (multi-select or single).
fn collect_drag_files(
    canvas: &MasonryCanvas,
    x: f64,
    y: f64,
    ctx: &AppContext,
) -> Option<Vec<gtk::gio::File>> {
    let imp = canvas.imp();
    let sel = imp.selection.get()?;
    let model = imp.model.get()?;
    let mut files = Vec::new();

    if imp.select_mode.get() {
        for i in 0..sel.n_items() {
            if sel.is_selected(i)
                && let Some(path) = resolve_drag_path(model, i, ctx)
            {
                files.push(gtk::gio::File::for_path(&path));
            }
        }
    }

    if files.is_empty() {
        let pos = canvas.hit_test(x, y)?;
        let path = resolve_drag_path(model, pos, ctx)?;
        files.push(gtk::gio::File::for_path(&path));
    }

    if files.is_empty() { None } else { Some(files) }
}

fn files_to_content_provider(files: Vec<gtk::gio::File>) -> gtk::gdk::ContentProvider {
    if files.len() == 1 {
        gtk::gdk::ContentProvider::for_value(&files[0].to_value())
    } else {
        let file_list = gtk::gdk::FileList::from_array(&files);
        gtk::gdk::ContentProvider::for_value(&file_list.to_value())
    }
}

/// Count selected items, or 1 if not in select mode.
fn selected_count(canvas: &MasonryCanvas) -> u32 {
    let imp = canvas.imp();
    if !imp.select_mode.get() {
        return 1;
    }
    imp.selection
        .get()
        .map(|s| (0..s.n_items()).filter(|i| s.is_selected(*i)).count() as u32)
        .unwrap_or(1)
}

/// Build a badge paintable showing the drag count.
fn build_drag_badge(count: u32) -> gtk::WidgetPaintable {
    let label = gtk::Label::builder()
        .label(format!("{count} files"))
        .css_classes(["mimick-drag-badge"])
        .build();
    let container = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["mimick-drag-badge"])
        .build();
    container.append(&label);
    gtk::WidgetPaintable::new(Some(&container))
}

fn connect_model_changes(canvas: &MasonryCanvas, model: &LibraryAssetModel) {
    let weak = canvas.downgrade();
    model.connect_items_changed(move |model, position, removed, added| {
        if let Some(canvas) = weak.upgrade() {
            handle_model_changed(&canvas, model, position, removed, added);
        }
    });
}

fn connect_selection_changes(canvas: &MasonryCanvas, selection: &gtk::MultiSelection) {
    let weak = canvas.downgrade();
    selection.connect_selection_changed(move |_, _, _| {
        if let Some(canvas) = weak.upgrade() {
            canvas.queue_draw();
        }
    });
}

fn handle_model_changed(
    canvas: &MasonryCanvas,
    model: &LibraryAssetModel,
    position: u32,
    removed: u32,
    added: u32,
) {
    let imp = canvas.imp();
    retain_failed_ids_for_model(imp, model);
    log::debug!(
        "masonry items_changed pos={} removed={} added={} model_n={}",
        position,
        removed,
        added,
        model.n_items(),
    );
    if added == 0 && removed > 0 && model.n_items() == 0 {
        return;
    }
    imp.invalidate_layout();
    canvas.queue_draw();
}

fn retain_failed_ids_for_model(imp: &imp::MasonryCanvas, model: &LibraryAssetModel) {
    let mut current_ids: HashSet<String> = HashSet::with_capacity(model.n_items() as usize);
    for i in 0..model.n_items() {
        if let Some(obj) = model.item(i).and_downcast::<AssetObject>() {
            current_ids.insert(obj.property::<String>("id"));
        }
    }
    imp.failed
        .borrow_mut()
        .retain(|id| current_ids.contains(id));
}

fn enable_select_mode_if_needed(imp: &imp::MasonryCanvas, _pos: u32) {
    if !imp.select_mode.get()
        && let Some(changer) = imp.select_mode_changer.borrow().clone()
    {
        (*changer)(true);
    }
}

fn primary_click_controller(canvas: &MasonryCanvas) -> gtk::GestureClick {
    let primary = gtk::GestureClick::new();
    primary.set_button(gtk::gdk::BUTTON_PRIMARY);
    let weak = canvas.downgrade();
    // Use `released` instead of `pressed` so a drag gesture that starts
    // from the same press doesn't also trigger lightbox activation. On mobile
    // the drag source is detached (see set_narrow), so the ScrolledWindow owns
    // touch drags and cancels this gesture on a real drag — a stationary tap
    // still fires `released` and opens the lightbox. (A press→release movement
    // threshold was tried here but broke taps: `connect_pressed` doesn't fire
    // reliably for touch, leaving the press point at (0,0) so every tap read as
    // a large drag and was discarded.)
    primary.connect_released(move |gesture, _, x, y| {
        if let Some(canvas) = weak.upgrade() {
            if canvas.imp().drag_active.get() {
                return;
            }
            canvas.handle_primary_click(gesture, x, y);
        }
    });
    primary
}

fn secondary_click_controller(canvas: &MasonryCanvas) -> gtk::GestureClick {
    let secondary = gtk::GestureClick::new();
    secondary.set_button(gtk::gdk::BUTTON_SECONDARY);
    let weak = canvas.downgrade();
    secondary.connect_pressed(move |_, _, x, y| {
        if let Some(canvas) = weak.upgrade() {
            canvas.handle_secondary_click(x, y);
        }
    });
    secondary
}

/// Long-press (touch/mouse hold) → enter select mode + anchor a range at the
/// pressed tile. On `pressed` we arm `range_drag_active`; the paired drag
/// gesture then extends the selection as the finger moves. A short delay gives
/// the press-and-lift = single-tap path room to win instead.
fn long_press_controller(canvas: &MasonryCanvas) -> gtk::GestureLongPress {
    let long_press = gtk::GestureLongPress::new();
    long_press.set_touch_only(false);
    // Snappy but distinct from a tap. Default ~500ms feels sluggish on a phone.
    long_press.set_delay_factor(0.6);
    let weak = canvas.downgrade();
    long_press.connect_pressed(move |gesture, x, y| {
        let Some(canvas) = weak.upgrade() else {
            return;
        };
        // Long-press claims the sequence so the tap gesture won't ALSO fire a
        // toggle/lightbox on release, and the ScrolledWindow won't treat the
        // subsequent motion as a scroll.
        gesture.set_state(gtk::EventSequenceState::Claimed);
        canvas.begin_range_select(x, y);
        canvas.queue_draw();
    });
    long_press
}

/// Drag gesture paired with the long-press. It only acts once `range_drag_active`
/// has been armed by a long-press — otherwise it stays passive so a plain
/// touch-drag scrolls the grid as before (the ScrolledWindow owns it). When
/// active it claims the sequence, live-updates the range selection, and drives
/// edge auto-scroll.
fn range_drag_controller(canvas: &MasonryCanvas) -> gtk::GestureDrag {
    let drag = gtk::GestureDrag::new();
    drag.set_button(gtk::gdk::BUTTON_PRIMARY);

    // Capture the press point so drag deltas map back to widget coordinates.
    let start = Rc::new(Cell::new((0.0_f64, 0.0_f64)));

    let weak = canvas.downgrade();
    let start_begin = start.clone();
    drag.connect_drag_begin(move |gesture, x, y| {
        start_begin.set((x, y));
        let Some(canvas) = weak.upgrade() else {
            return;
        };
        if canvas.imp().range_drag_active.get() {
            // A long-press already armed us: own this sequence.
            gesture.set_state(gtk::EventSequenceState::Claimed);
        }
    });

    let weak = canvas.downgrade();
    let start_update = start.clone();
    drag.connect_drag_update(move |gesture, ox, oy| {
        let Some(canvas) = weak.upgrade() else {
            return;
        };
        if !canvas.imp().range_drag_active.get() {
            return; // not our sequence — let the ScrolledWindow scroll.
        }
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let (sx, sy) = start_update.get();
        let (x, y) = (sx + ox, sy + oy);
        canvas.update_range_select(x, y);
        canvas.update_autoscroll(y);
        canvas.queue_draw();
    });

    let weak = canvas.downgrade();
    drag.connect_drag_end(move |_gesture, _ox, _oy| {
        if let Some(canvas) = weak.upgrade() {
            canvas.end_range_select();
        }
    });

    // Also tear down if the sequence is cancelled (e.g. touch lifted off-window).
    let weak = canvas.downgrade();
    drag.connect_cancel(move |_gesture, _seq| {
        if let Some(canvas) = weak.upgrade() {
            canvas.end_range_select();
        }
    });

    drag
}

fn toggle_selection(sel: &gtk::MultiSelection, pos: u32) {
    if sel.is_selected(pos) {
        sel.unselect_item(pos);
    } else {
        sel.select_item(pos, false);
    }
}

fn select_range(sel: &gtk::MultiSelection, lo: u32, hi: u32) {
    let count = hi - lo + 1;
    sel.select_range(lo, count, false);
}

/// Resolve the drag-export path for the asset at `pos`.
///
/// Priority: local path > drag export cache (with original filename)
///         > lightbox preview cache (hardlinked to proper name)
///         > on-demand download.
fn resolve_drag_path(
    model: &LibraryAssetModel,
    pos: u32,
    ctx: &AppContext,
) -> Option<std::path::PathBuf> {
    let obj = model.item(pos).and_downcast::<AssetObject>()?;
    let local_path = obj.property::<String>("local-path");
    if !local_path.is_empty() {
        let p = std::path::PathBuf::from(&local_path);
        if p.exists() {
            return Some(p);
        }
    }

    let remote_id = obj.property::<String>("remote-id");
    let filename = obj.property::<String>("filename");
    if remote_id.is_empty() || filename.is_empty() {
        return None;
    }

    let export_path = export_cache_path(&remote_id, &filename)?;
    if export_path.exists() {
        return Some(export_path);
    }

    if try_link_from_preview(&remote_id, &filename, &export_path) {
        return Some(export_path);
    }

    if download_for_export(ctx, &remote_id, &filename, &export_path) {
        Some(export_path)
    } else {
        None
    }
}

fn export_cache_path(remote_id: &str, filename: &str) -> Option<std::path::PathBuf> {
    let export_dir = crate::profile::cache_dir()?.join("drag_export");
    let _ = std::fs::create_dir_all(&export_dir);
    let safe_name = format!("{}_{}", &remote_id[..8.min(remote_id.len())], filename);
    Some(export_dir.join(&safe_name))
}

fn try_link_from_preview(remote_id: &str, filename: &str, export_path: &std::path::Path) -> bool {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty())
        .unwrap_or("bin");
    let preview_dir = match crate::profile::cache_dir() {
        Some(p) => p.join("preview"),
        None => return false,
    };
    let cached = preview_dir.join(format!("{remote_id}.{ext}"));

    cached.exists()
        && (std::fs::hard_link(&cached, export_path).is_ok()
            || std::fs::copy(&cached, export_path).is_ok())
}

fn download_for_export(
    ctx: &AppContext,
    remote_id: &str,
    filename: &str,
    export_path: &std::path::Path,
) -> bool {
    log::debug!("Drag export: downloading {} ({})", remote_id, filename);
    let api = ctx.api_client.clone();
    let id = remote_id.to_string();
    let dest = export_path.to_path_buf();

    let downloaded = std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                log::error!("Failed to create tokio runtime for drag download: {}", e);
                return false;
            }
        };
        match rt.block_on(api.download_original_to_file(&id, &dest, None)) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("Drag export download failed for {}: {}", id, e);
                let _ = std::fs::remove_file(&dest);
                false
            }
        }
    })
    .join()
    .unwrap_or(false);

    downloaded && export_path.exists()
}
