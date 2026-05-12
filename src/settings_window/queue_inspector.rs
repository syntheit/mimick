//! Queue inspector dialog -- browse failed tasks and recent queue activity.

use std::path::Path;
use std::sync::Arc;

use adw::prelude::*;
use glib::clone;
use gtk::prelude::*;
use gtk::{Box, Button, ListBox, Orientation, ScrolledWindow};
use libadwaita as adw;

use crate::queue_manager::QueueManager;
use crate::watch_path_display::display_watch_path;

pub fn show_queue_inspector(
    parent: &impl gtk::prelude::IsA<gtk::Window>,
    queue_manager: Arc<QueueManager>,
) {
    let dialog = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Queue Inspector")
        .default_width(900)
        .default_height(680)
        .build();
    let content = Box::builder()
        .orientation(Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    dialog.set_content(Some(&content));

    let actions = Box::builder()
        .orientation(Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    content.append(&actions);

    let retry_all_btn = Button::builder().label("Retry All Failed").build();
    let clear_failed_btn = Button::builder().label("Clear Failed Queue").build();
    actions.append(&retry_all_btn);
    actions.append(&clear_failed_btn);

    let failed_group = adw::PreferencesGroup::builder()
        .title("Failed Retry Queue")
        .build();
    content.append(&failed_group);

    let failed_tasks = queue_manager.failed_tasks();
    if failed_tasks.is_empty() {
        failed_group.add(
            &adw::ActionRow::builder()
                .title("No failed items")
                .subtitle("The retry queue is currently empty.")
                .build(),
        );
    } else {
        for task in failed_tasks {
            let row = adw::ActionRow::builder()
                .title(
                    Path::new(&task.path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(task.path.as_str()),
                )
                .subtitle(&task.path)
                .subtitle_lines(3)
                .build();
            let retry_btn = Button::builder().label("Retry").build();
            let task_path = task.path.clone();
            let qm = queue_manager.clone();
            retry_btn.connect_clicked(move |btn| {
                btn.set_sensitive(false);
                let qm = qm.clone();
                let task_path = task_path.clone();
                glib::MainContext::default().spawn_local(async move {
                    let _ = qm.retry_failed_path(&task_path).await;
                });
            });
            row.add_suffix(&retry_btn);
            failed_group.add(&row);
        }
    }

    let events_group = adw::PreferencesGroup::builder()
        .title("Recent Queue Activity")
        .build();
    content.append(&events_group);

    let events_scroll = ScrolledWindow::builder()
        .min_content_height(340)
        .vexpand(true)
        .build();
    let events_list = ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(vec!["boxed-list".to_string()])
        .build();
    events_scroll.set_child(Some(&events_list));
    events_group.add(&events_scroll);

    for event in queue_manager.recent_events() {
        let row = adw::ActionRow::builder()
            .title(
                Path::new(&event.path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(event.path.as_str()),
            )
            .subtitle(format!(
                "{} | attempts={}{}",
                event.status,
                event.attempts,
                event
                    .detail
                    .as_ref()
                    .map(|detail| format!(" | {}", detail))
                    .unwrap_or_default()
            ))
            .subtitle_lines(4)
            .build();
        row.add_prefix(
            &gtk::Label::builder()
                .label(display_watch_path(&event.path))
                .wrap(true)
                .halign(gtk::Align::Start)
                .build(),
        );
        events_list.append(&row);
    }

    let qm_retry_all = queue_manager.clone();
    retry_all_btn.connect_clicked(move |btn| {
        btn.set_sensitive(false);
        let qm = qm_retry_all.clone();
        glib::MainContext::default().spawn_local(async move {
            let _ = qm.retry_all_failed().await;
        });
    });

    let qm_clear = queue_manager.clone();
    clear_failed_btn.connect_clicked(move |_| {
        let _ = qm_clear.clear_failed();
    });

    let close_btn = Button::builder().label("Close").build();
    close_btn.connect_clicked(clone!(
        #[weak]
        dialog,
        move |_| {
            dialog.close();
        }
    ));
    content.append(&close_btn);
    dialog.present();
}

pub(super) fn show_about_dialog(parent: &impl gtk::prelude::IsA<gtk::Widget>) {
    let about = adw::AboutDialog::builder()
        .application_name("Mimick")
        .application_icon("dev.nicx.mimick")
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("Nick Cardoso")
        .website("https://github.com/nicx17/mimick")
        .issue_url("https://github.com/nicx17/mimick/issues")
        .license_type(gtk::License::Gpl30)
        .build();

    about.add_credit_section(
        Some("Icon Design"),
        &["Round Icons https://unsplash.com/illustrations/a-white-and-orange-flower-on-a-white-background-IkQ_WrJzZOM"],
    );

    let third_party =
        glib::markup_escape_text(include_str!("../../THIRD_PARTY_LICENSES_SUMMARY.txt"));
    about.add_legal_section(
        "Third-party Licenses",
        None,
        gtk::License::Custom,
        Some(&third_party),
    );

    about.present(Some(parent));
}
