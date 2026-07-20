//! Galleries settings section.
//!
//! Galleries are the folders whose LOCAL photos are displayed in the Photos
//! timeline (each with its cloud sync-state badge). This is intentionally
//! SEPARATE from watch folders (`watch_paths`), which control backup/upload —
//! seeing a folder here does not back it up.
//!
//! The UI mirrors the watch-folder list but is much simpler: a gallery is just
//! a path (no album binding, no rules), so each row is a plain `ActionRow`
//! with a remove button, plus an "Add Folder" button below.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::prelude::*;
use gtk::{Button, FileDialog, ListBox};
use libadwaita as adw;

use crate::app_context::AppContext;
use crate::watch_path_display::{display_watch_path, display_watch_path_inline};

/// Build the "Galleries" preferences group and wire add/remove persistence.
///
/// `window` is the settings dialog surface, used only to resolve the toplevel
/// `gtk::Window` that `FileDialog` needs as a parent.
///
/// Changes are persisted to config immediately. The timeline reads
/// `config.galleries` live via `enumerate_galleries`, so edits take effect on
/// the next Photos load (tab re-entry or refresh) without a cross-window
/// signal.
pub fn build_galleries_group(
    settings_page: &adw::PreferencesPage,
    ctx: &Arc<AppContext>,
    window: &adw::Dialog,
) {
    let group = adw::PreferencesGroup::builder()
        .title("Galleries")
        .description("Folders whose local photos appear in the timeline. Separate from backup.")
        .build();
    settings_page.add(&group);

    let list = ListBox::builder()
        .margin_top(12)
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(vec!["boxed-list".to_string()])
        .build();
    group.add(&list);

    let add_btn = Button::builder()
        .label("Add Folder")
        .margin_top(12)
        .build();
    group.add(&add_btn);

    // Track the widget rows so a removal can drop the exact row.
    let rows: Rc<RefCell<Vec<(String, adw::ActionRow)>>> = Rc::new(RefCell::new(Vec::new()));

    for path in ctx.config.read().gallery_paths() {
        add_gallery_row(&list, &path, ctx, &rows);
    }

    let ctx_for_add = ctx.clone();
    let list_for_add = list.clone();
    let rows_for_add = rows.clone();
    let window_for_add = window.clone();
    add_btn.connect_clicked(move |_| {
        let dialog = FileDialog::builder().title("Select Gallery Folder").build();
        let ctx = ctx_for_add.clone();
        let list = list_for_add.clone();
        let rows = rows_for_add.clone();

        // FileDialog needs a gtk::Window parent, not a Widget; adw::Dialog is
        // not a Window, so resolve the toplevel window hosting the dialog.
        let file_parent = window_for_add.root().and_downcast::<gtk::Window>();
        dialog.select_folder(
            file_parent.as_ref(),
            gtk::gio::Cancellable::NONE,
            move |res| {
                if let Ok(file) = res
                    && let Some(path) = file.path()
                {
                    let path_str = path.to_string_lossy().to_string();
                    let added = {
                        let mut cfg = ctx.config.write();
                        let added = cfg.add_gallery(&path_str);
                        if added {
                            cfg.save();
                        }
                        added
                    };
                    if added {
                        add_gallery_row(&list, &path_str, &ctx, &rows);
                    }
                }
            },
        );
    });
}

/// Append a single gallery folder row (folder name + path subtitle + remove).
fn add_gallery_row(
    list: &ListBox,
    path: &str,
    ctx: &Arc<AppContext>,
    rows: &Rc<RefCell<Vec<(String, adw::ActionRow)>>>,
) {
    let row = adw::ActionRow::builder()
        .title(display_watch_path(path))
        .subtitle(display_watch_path_inline(path))
        .title_lines(1)
        .subtitle_lines(1)
        .build();

    let remove_btn = Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Remove from galleries")
        .valign(gtk::Align::Center)
        .css_classes(vec!["flat".to_string()])
        .build();
    row.add_suffix(&remove_btn);

    let ctx_for_remove = ctx.clone();
    let list_for_remove = list.clone();
    let rows_for_remove = rows.clone();
    let path_owned = path.to_string();
    let row_weak = row.downgrade();
    remove_btn.connect_clicked(move |_| {
        {
            let mut cfg = ctx_for_remove.config.write();
            if cfg.remove_gallery(&path_owned) {
                cfg.save();
            }
        }
        if let Some(row) = row_weak.upgrade() {
            list_for_remove.remove(&row);
        }
        rows_for_remove.borrow_mut().retain(|(p, _)| p != &path_owned);
    });

    list.append(&row);
    rows.borrow_mut().push((path.to_string(), row));
}
