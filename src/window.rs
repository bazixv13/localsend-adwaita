use crate::application::APP_ID;
use crate::core::{Core, Event, IncomingFileInfo, IncomingRequest};
use crate::history::{HistoryEntry, HistoryItem, Resolution};
use crate::sender;
use crate::util;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use libadwaita as adw;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// One notification id for every transfer and message, so a new one always
/// replaces the previous popup instead of stacking another entry.
const NOTIFICATION_ID: &str = "localsend";

pub fn build(app: &adw::Application, core: Arc<Core>) -> adw::ApplicationWindow {
    let receiver = core.subscribe();
    let ui = Rc::new(Ui::new(app, core.clone()));
    ACTIVE_UI.with(|slot| *slot.borrow_mut() = Some(std::rc::Rc::downgrade(&ui)));
    ui.wire(app);
    ui.spawn_event_loop(receiver);
    ui.refresh_devices();
    ui.show_empty();
    ui.window.clone()
}

struct Ui {
    window: adw::ApplicationWindow,
    core: Arc<Core>,

    split: adw::OverlaySplitView,
    sidebar_toggle: gtk::ToggleButton,
    window_title: adw::WindowTitle,
    sidebar_stack: gtk::Stack,
    device_list: gtk::ListBox,
    device_keys: RefCell<Vec<String>>,
    updating_list: Cell<bool>,

    content_stack: adw::ViewStack,

    device_avatar: adw::Avatar,
    device_name: gtk::Label,
    device_meta: gtk::Label,
    send_button: gtk::Button,
    send_folder_button: gtk::Button,
    send_text_button: gtk::Button,
    drop_zone: gtk::Box,

    transfer_title: gtk::Label,
    transfer_file: gtk::Button,
    transfer_progress: gtk::ProgressBar,
    transfer_detail: gtk::Label,
    transfer_cancel: gtk::Button,

    transfer_indicator: gtk::MenuButton,
    indicator_ring: gtk::DrawingArea,
    indicator_fraction: Rc<Cell<f64>>,
    indicator_title: gtk::Label,
    indicator_detail: gtk::Label,
    indicator_bar: gtk::ProgressBar,
    indicator_show: gtk::Button,
    indicator_cancel: gtk::Button,

    result_page: adw::StatusPage,
    result_primary: gtk::Button,
    result_secondary: gtk::Button,
    result_action: RefCell<ResultAction>,

    home_drop: gtk::Box,
    add_files_button: gtk::Button,
    add_folder_button: gtk::Button,
    queue_text_button: gtk::Button,
    queue_list: gtk::ListBox,
    queue_section: gtk::Box,

    search_status: gtk::Box,
    search_spinner: gtk::Spinner,

    decision_title: gtk::Label,
    decision_body: gtk::Label,
    decision_accept: gtk::Button,
    decision_decline: gtk::Button,

    toasts: adw::ToastOverlay,
    banner: adw::Banner,

    selected: RefCell<Option<String>>,
    transfer: RefCell<Option<TransferState>>,
    device_revision: Cell<u64>,

    queue: RefCell<Vec<QueueItem>>,
    /// Messages that follow the queued files once they have been sent.
    queued_texts: RefCell<Vec<String>>,
    /// The device the queue is currently being sent to.
    queue_device: RefCell<Option<String>>,

    background: RefCell<bool>,
    notification_percent: Cell<u64>,
    notification_at: Cell<Option<Instant>>,
    pending_text: RefCell<Option<PendingText>>,
    pending_decision: RefCell<Option<PendingDecision>>,
    history_dialog: RefCell<Option<(adw::PreferencesDialog, adw::PreferencesPage)>>,
    history_groups: RefCell<Vec<adw::PreferencesGroup>>,
    history_expanded: RefCell<HashSet<String>>,
    /// Set while a history rebuild is queued, so a burst of events (a transfer
    /// records one entry per file) collapses into a single rebuild.
    history_refresh_queued: Cell<bool>,
}

/// Something waiting for a device before it can be sent.
#[derive(Clone)]
enum QueueItem {
    Files {
        paths: Vec<PathBuf>,
        label: String,
        detail: String,
    },
    Text {
        text: String,
        label: String,
        detail: String,
    },
}

impl QueueItem {
    fn files(paths: Vec<PathBuf>) -> Self {
        let label = match paths.as_slice() {
            [path] => path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "Folder".to_string()),
            [first, ..] => format!(
                "{} and {} more",
                first
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Folder".to_string()),
                paths.len() - 1
            ),
            [] => "Files".to_string(),
        };
        let mut bytes = 0u64;
        let mut folders = 0usize;
        for path in &paths {
            match std::fs::metadata(path) {
                Ok(meta) if meta.is_file() => bytes += meta.len(),
                Ok(_) => folders += 1,
                Err(_) => {}
            }
        }
        let mut parts = Vec::new();
        if paths.len() > 1 {
            parts.push(format!("{} items", paths.len()));
        }
        if bytes > 0 {
            parts.push(util::human_bytes(bytes));
        }
        if folders > 0 {
            parts.push(format!(
                "{} folder{}",
                folders,
                if folders == 1 { "" } else { "s" }
            ));
        }
        if parts.is_empty() {
            parts.push("Files".to_string());
        }
        Self::Files {
            paths,
            label,
            detail: parts.join(" · "),
        }
    }

    fn text(text: String) -> Self {
        Self::Text {
            label: "Message".to_string(),
            detail: summarize(&text),
            text,
        }
    }
}

/// A message delivered while the window was hidden. It is kept for an hour so
/// the notification stays useful after the transfer itself is long gone.
struct PendingText {
    alias: String,
    messages: Vec<String>,
    received: Instant,
}

/// A transfer waiting for the user to accept or decline it from a notification.
struct PendingDecision {
    session_id: String,
    alias: String,
    files: usize,
    total_bytes: u64,
    files_label: String,
    sender: tokio::sync::oneshot::Sender<bool>,
}

thread_local! {
    static ACTIVE_UI: RefCell<Option<std::rc::Weak<Ui>>> = const { RefCell::new(None) };
}

/// Opens the message a notification announced, presenting the window first.
pub fn flush_pending_text() {
    let ui = ACTIVE_UI.with(|slot| slot.borrow().as_ref().and_then(std::rc::Weak::upgrade));
    if let Some(ui) = ui {
        ui.flush_pending_text();
    }
}

/// Queues files handed over by the file manager, for example GNOME Files'
/// "Send with LocalSend" menu item or "Open With".
pub fn queue_opened_files(paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    let ui = ACTIVE_UI.with(|slot| slot.borrow().as_ref().and_then(std::rc::Weak::upgrade));
    if let Some(ui) = ui {
        ui.window.present();
        ui.queue_files(paths);
    }
}

/// Answers an accept/decline request coming from a notification action.
pub fn resolve_incoming_transfer(accept: bool, session_id: &str) {
    let ui = ACTIVE_UI.with(|slot| slot.borrow().as_ref().and_then(std::rc::Weak::upgrade));
    if let Some(ui) = ui {
        ui.resolve_incoming_transfer(accept, session_id);
    }
}

#[derive(Clone)]
enum ResultAction {
    None,
    Back,
    Sent { device: String },
    MessageSent { device: String },
    Received { directory: PathBuf },
}

struct TransferState {
    kind: TransferKind,
    cancel: Arc<AtomicBool>,
    session_id: Option<String>,
    total: u64,
    done: u64,
    /// Stable label for multi-file receives (avoids flashing per-file names).
    receive_label: Option<String>,
}

enum TransferKind {
    Sending,
    Receiving,
}

impl Ui {
    fn new(app: &adw::Application, core: Arc<Core>) -> Self {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("LocalSend")
            .default_width(1000)
            .default_height(720)
            .build();

        // Home page: the queue lives here, so files can be collected before a
        // device has even shown up.
        let home_icon = gtk::Image::from_icon_name("send-to-symbolic");
        home_icon.set_pixel_size(88);
        home_icon.add_css_class("dim-label");
        let home_title = gtk::Label::new(Some("Send Files"));
        home_title.add_css_class("title-1");
        let home_description = gtk::Label::new(Some(
            "Drop files or folders here, or add them below. Nothing is sent until you pick a nearby device and confirm.",
        ));
        home_description.add_css_class("dim-label");
        home_description.set_wrap(true);
        home_description.set_justify(gtk::Justification::Center);
        home_description.set_max_width_chars(42);

        let add_files_button = gtk::Button::builder()
            .label("Add Files…")
            .icon_name("document-send-symbolic")
            .build();
        add_files_button.add_css_class("pill");
        add_files_button.add_css_class("suggested-action");
        let add_folder_button = gtk::Button::builder()
            .label("Add Folder…")
            .icon_name("folder-symbolic")
            .build();
        add_folder_button.add_css_class("pill");
        let queue_text_button = gtk::Button::builder()
            .label("Add Text…")
            .icon_name("chat-message-new-symbolic")
            .build();
        queue_text_button.add_css_class("pill");
        let home_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        home_buttons.set_halign(gtk::Align::Center);
        home_buttons.append(&add_files_button);
        home_buttons.append(&add_folder_button);
        home_buttons.append(&queue_text_button);

        let home_drop = gtk::Box::new(gtk::Orientation::Vertical, 6);
        home_drop.add_css_class("drop-zone");
        home_drop.set_hexpand(true);
        home_drop.set_height_request(104);
        home_drop.set_valign(gtk::Align::Center);
        let home_drop_icon = gtk::Image::from_icon_name("document-open-symbolic");
        home_drop_icon.set_pixel_size(28);
        home_drop_icon.add_css_class("dim-label");
        let home_drop_label = gtk::Label::new(Some("Drop files or folders to queue them"));
        home_drop_label.add_css_class("dim-label");
        home_drop.append(&home_drop_icon);
        home_drop.append(&home_drop_label);

        let queue_heading = gtk::Label::new(Some("Queue"));
        queue_heading.add_css_class("heading");
        queue_heading.set_halign(gtk::Align::Start);
        let queue_list = gtk::ListBox::new();
        queue_list.set_selection_mode(gtk::SelectionMode::None);
        queue_list.add_css_class("boxed-list");
        let queue_section = gtk::Box::new(gtk::Orientation::Vertical, 8);
        queue_section.append(&queue_heading);
        queue_section.append(&queue_list);
        queue_section.set_visible(false);

        let home_box = gtk::Box::new(gtk::Orientation::Vertical, 14);
        home_box.set_halign(gtk::Align::Center);
        home_box.set_valign(gtk::Align::Center);
        home_box.set_margin_top(36);
        home_box.set_margin_bottom(36);
        home_box.set_margin_start(24);
        home_box.set_margin_end(24);
        home_box.append(&home_icon);
        home_box.append(&home_title);
        home_box.append(&home_description);
        home_box.append(&home_buttons);
        home_box.append(&home_drop);
        home_box.append(&queue_section);

        let home_clamp = adw::Clamp::new();
        home_clamp.set_maximum_size(620);
        home_clamp.set_tightening_threshold(480);
        home_clamp.set_child(Some(&home_box));
        let home_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&home_clamp)
            .build();

        // Sidebar
        let sidebar_heading = gtk::Label::new(Some("Nearby Devices"));
        sidebar_heading.add_css_class("heading");
        sidebar_heading.set_halign(gtk::Align::Start);
        sidebar_heading.set_margin_top(18);
        sidebar_heading.set_margin_start(18);
        sidebar_heading.set_margin_end(18);
        sidebar_heading.set_margin_bottom(6);

        let device_list = gtk::ListBox::new();
        device_list.set_selection_mode(gtk::SelectionMode::Single);
        device_list.add_css_class("boxed-list");
        device_list.set_margin_start(12);
        device_list.set_margin_end(12);
        device_list.set_margin_bottom(12);
        device_list.set_valign(gtk::Align::Start);

        let device_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&device_list)
            .build();

        let sidebar_empty = gtk::Box::new(gtk::Orientation::Vertical, 10);
        sidebar_empty.set_valign(gtk::Align::Center);
        sidebar_empty.set_halign(gtk::Align::Center);
        sidebar_empty.set_margin_start(24);
        sidebar_empty.set_margin_end(24);
        sidebar_empty.set_margin_bottom(36);

        let empty_icon = gtk::Image::from_icon_name("network-wireless-offline-symbolic");
        empty_icon.set_pixel_size(48);
        empty_icon.add_css_class("dim-label");
        let empty_title = gtk::Label::new(Some("No Devices Found"));
        empty_title.add_css_class("title-4");
        let empty_description = gtk::Label::new(Some(
            "Make sure LocalSend is open on another device on this network. Devices appear automatically.",
        ));
        empty_description.add_css_class("dim-label");
        empty_description.set_wrap(true);
        empty_description.set_justify(gtk::Justification::Center);
        empty_description.set_max_width_chars(28);

        sidebar_empty.append(&empty_icon);
        sidebar_empty.append(&empty_title);
        sidebar_empty.append(&empty_description);

        let sidebar_stack = gtk::Stack::new();
        sidebar_stack.set_vexpand(true);
        sidebar_stack.add_named(&sidebar_empty, Some("empty"));
        sidebar_stack.add_named(&device_scroller, Some("list"));
        sidebar_stack.set_visible_child_name("empty");

        // Searching status, pinned to the bottom of the sidebar.
        let search_spinner = gtk::Spinner::new();
        search_spinner.set_size_request(16, 16);
        search_spinner.set_valign(gtk::Align::Center);
        let search_label = gtk::Label::new(Some("Searching for devices…"));
        search_label.add_css_class("caption");
        search_label.add_css_class("dim-label");
        let search_status = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        search_status.set_halign(gtk::Align::Center);
        search_status.set_margin_top(6);
        search_status.set_margin_bottom(18);
        search_status.append(&search_spinner);
        search_status.append(&search_label);

        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar.append(&sidebar_heading);
        sidebar.append(&sidebar_stack);
        sidebar.append(&search_status);
        sidebar.set_size_request(300, -1);

        // Device detail
        let device_avatar = adw::Avatar::new(88, Some("?"), true);
        device_avatar.set_halign(gtk::Align::Center);
        let device_name = gtk::Label::new(None);
        device_name.add_css_class("title-1");
        device_name.set_justify(gtk::Justification::Center);
        device_name.set_wrap(true);
        let device_meta = gtk::Label::new(None);
        device_meta.add_css_class("dim-label");
        device_meta.set_justify(gtk::Justification::Center);

        let send_button = gtk::Button::builder()
            .label("Send Files…")
            .icon_name("document-send-symbolic")
            .halign(gtk::Align::Center)
            .build();
        send_button.add_css_class("suggested-action");
        send_button.add_css_class("pill");

        let send_folder_button = gtk::Button::builder()
            .label("Send Folder…")
            .icon_name("folder-symbolic")
            .halign(gtk::Align::Center)
            .build();
        send_folder_button.add_css_class("pill");
        let send_text_button = gtk::Button::builder()
            .label("Send Text…")
            .icon_name("chat-message-new-symbolic")
            .halign(gtk::Align::Center)
            .build();
        send_text_button.add_css_class("pill");
        let send_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        send_buttons.set_halign(gtk::Align::Center);
        send_buttons.append(&send_button);
        send_buttons.append(&send_folder_button);
        send_buttons.append(&send_text_button);

        let drop_zone = gtk::Box::new(gtk::Orientation::Vertical, 6);
        drop_zone.add_css_class("drop-zone");
        drop_zone.set_halign(gtk::Align::Center);
        drop_zone.set_margin_top(10);
        drop_zone.set_size_request(320, -1);
        let drop_icon = gtk::Image::from_icon_name("document-open-symbolic");
        drop_icon.set_pixel_size(28);
        drop_icon.add_css_class("dim-label");
        let drop_label = gtk::Label::new(Some("or drop files and folders here"));
        drop_label.add_css_class("dim-label");
        drop_zone.append(&drop_icon);
        drop_zone.append(&drop_label);

        let device_box = gtk::Box::new(gtk::Orientation::Vertical, 16);
        device_box.set_halign(gtk::Align::Center);
        device_box.set_valign(gtk::Align::Center);
        device_box.set_margin_top(36);
        device_box.set_margin_bottom(36);
        device_box.set_margin_start(24);
        device_box.set_margin_end(24);
        device_box.append(&device_avatar);
        device_box.append(&device_name);
        device_box.append(&device_meta);
        device_box.append(&send_buttons);
        device_box.append(&drop_zone);

        let device_clamp = adw::Clamp::new();
        device_clamp.set_maximum_size(560);
        device_clamp.set_tightening_threshold(420);
        device_clamp.set_child(Some(&device_box));

        // Transfer progress
        let transfer_title = gtk::Label::new(None);
        transfer_title.add_css_class("title-2");
        transfer_title.set_wrap(true);
        transfer_title.set_justify(gtk::Justification::Center);
        let transfer_file = gtk::Button::with_label("");
        transfer_file.add_css_class("pill");
        transfer_file.add_css_class("flat");
        transfer_file.set_halign(gtk::Align::Center);
        transfer_file.set_tooltip_text(Some("Show file names"));
        let transfer_progress = gtk::ProgressBar::new();
        transfer_progress.add_css_class("transfer-progress");
        transfer_progress.set_show_text(false);
        transfer_progress.set_fraction(0.0);
        transfer_progress.set_margin_top(6);
        let transfer_detail = gtk::Label::new(None);
        transfer_detail.add_css_class("caption");
        transfer_detail.add_css_class("dim-label");
        transfer_detail.set_halign(gtk::Align::Center);
        let transfer_cancel = gtk::Button::with_label("Cancel");
        transfer_cancel.add_css_class("pill");
        transfer_cancel.add_css_class("destructive-action");
        transfer_cancel.set_halign(gtk::Align::Center);

        let transfer_box = gtk::Box::new(gtk::Orientation::Vertical, 16);
        transfer_box.set_halign(gtk::Align::Center);
        transfer_box.set_valign(gtk::Align::Center);
        transfer_box.set_width_request(420);
        transfer_box.set_margin_top(36);
        transfer_box.set_margin_bottom(36);
        transfer_box.set_margin_start(24);
        transfer_box.set_margin_end(24);
        transfer_box.append(&transfer_title);
        transfer_box.append(&transfer_file);
        transfer_box.append(&transfer_progress);
        transfer_box.append(&transfer_detail);
        transfer_box.append(&transfer_cancel);

        let transfer_clamp = adw::Clamp::new();
        transfer_clamp.set_maximum_size(560);
        transfer_clamp.set_child(Some(&transfer_box));

        // Result
        let result_page = adw::StatusPage::builder()
            .icon_name("emblem-ok-symbolic")
            .title("Files Sent")
            .build();
        let result_primary = gtk::Button::with_label("Send More");
        result_primary.add_css_class("suggested-action");
        result_primary.add_css_class("pill");
        let result_secondary = gtk::Button::with_label("Done");
        result_secondary.add_css_class("pill");
        let result_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        result_buttons.set_halign(gtk::Align::Center);
        result_buttons.append(&result_primary);
        result_buttons.append(&result_secondary);
        result_page.set_child(Some(&result_buttons));

        // Incoming request waiting for an answer, shown in the window when it
        // is visible instead of as a notification.
        let decision_icon = gtk::Image::from_icon_name("document-save-symbolic");
        decision_icon.set_pixel_size(64);
        decision_icon.add_css_class("dim-label");
        let decision_title = gtk::Label::new(None);
        decision_title.add_css_class("title-2");
        decision_title.set_wrap(true);
        decision_title.set_justify(gtk::Justification::Center);
        let decision_body = gtk::Label::new(None);
        decision_body.add_css_class("dim-label");
        decision_body.set_wrap(true);
        decision_body.set_justify(gtk::Justification::Center);
        let decision_accept = gtk::Button::with_label("Accept");
        decision_accept.add_css_class("pill");
        decision_accept.add_css_class("suggested-action");
        let decision_decline = gtk::Button::with_label("Decline");
        decision_decline.add_css_class("pill");
        decision_decline.add_css_class("destructive-action");
        let decision_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        decision_buttons.set_halign(gtk::Align::Center);
        decision_buttons.append(&decision_decline);
        decision_buttons.append(&decision_accept);
        let decision_box = gtk::Box::new(gtk::Orientation::Vertical, 14);
        decision_box.set_halign(gtk::Align::Center);
        decision_box.set_valign(gtk::Align::Center);
        decision_box.set_margin_top(36);
        decision_box.set_margin_bottom(36);
        decision_box.set_margin_start(24);
        decision_box.set_margin_end(24);
        decision_box.append(&decision_icon);
        decision_box.append(&decision_title);
        decision_box.append(&decision_body);
        decision_box.append(&decision_buttons);
        let decision_clamp = adw::Clamp::new();
        decision_clamp.set_maximum_size(560);
        decision_clamp.set_child(Some(&decision_box));

        let content_stack = adw::ViewStack::new();
        content_stack.set_vexpand(true);
        content_stack.set_hexpand(true);
        content_stack.add_named(&home_scroller, Some("home"));
        content_stack.add_named(&device_clamp, Some("device"));
        content_stack.add_named(&transfer_clamp, Some("transfer"));
        content_stack.add_named(&decision_clamp, Some("decision"));
        content_stack.add_named(&result_page, Some("result"));
        content_stack.set_visible_child_name("home");

        let split = adw::OverlaySplitView::new();
        split.add_css_class("rounded-split");
        split.set_sidebar(Some(&sidebar));
        split.set_content(Some(&content_stack));
        split.set_min_sidebar_width(280.0);
        split.set_max_sidebar_width(360.0);
        split.set_show_sidebar(true);

        // Header
        let menu = gio::Menu::new();
        menu.append(Some("History"), Some("win.history"));
        menu.append(Some("Settings"), Some("win.settings"));
        menu.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
        menu.append(Some("About LocalSend"), Some("win.about"));
        menu.append(Some("Quit"), Some("app.quit"));
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .tooltip_text("Main Menu")
            .build();

        let sidebar_toggle = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-symbolic")
            .tooltip_text("Toggle Sidebar")
            .active(true)
            .build();
        sidebar_toggle.add_css_class("flat");

        // Header transfer indicator: a small ring showing the current
        // transfer's progress; clicking it opens a popover with details.
        let indicator_fraction = Rc::new(Cell::new(0.0f64));
        let indicator_ring = gtk::DrawingArea::new();
        indicator_ring.set_content_width(16);
        indicator_ring.set_content_height(16);
        indicator_ring.set_valign(gtk::Align::Center);
        {
            let fraction = indicator_fraction.clone();
            indicator_ring.set_draw_func(move |area, cr, width, height| {
                let size = f64::from(width.min(height));
                let line = (size / 7.0).max(2.0);
                let radius = (size - line) / 2.0;
                let center_x = f64::from(width) / 2.0;
                let center_y = f64::from(height) / 2.0;
                let color = area.color();
                cr.set_line_width(line);
                cr.set_source_rgba(
                    f64::from(color.red()),
                    f64::from(color.green()),
                    f64::from(color.blue()),
                    0.25,
                );
                cr.arc(center_x, center_y, radius, 0.0, std::f64::consts::TAU);
                let _ = cr.stroke();
                let fraction = fraction.get().clamp(0.0, 1.0);
                if fraction > 0.0 {
                    cr.set_source_rgba(
                        f64::from(color.red()),
                        f64::from(color.green()),
                        f64::from(color.blue()),
                        f64::from(color.alpha()),
                    );
                    cr.arc(
                        center_x,
                        center_y,
                        radius,
                        -std::f64::consts::FRAC_PI_2,
                        -std::f64::consts::FRAC_PI_2 + std::f64::consts::TAU * fraction,
                    );
                    let _ = cr.stroke();
                }
            });
        }

        let indicator_title = gtk::Label::new(None);
        indicator_title.add_css_class("title-5");
        indicator_title.set_xalign(0.0);
        let indicator_detail = gtk::Label::new(None);
        indicator_detail.add_css_class("caption");
        indicator_detail.add_css_class("dim-label");
        indicator_detail.set_xalign(0.0);
        indicator_detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let indicator_bar = gtk::ProgressBar::new();
        indicator_bar.set_show_text(false);
        let indicator_show = gtk::Button::with_label("Show Transfer");
        indicator_show.add_css_class("flat");
        let indicator_cancel = gtk::Button::with_label("Cancel");
        indicator_cancel.add_css_class("pill");
        indicator_cancel.add_css_class("destructive-action");
        let indicator_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        indicator_buttons.set_halign(gtk::Align::End);
        indicator_buttons.append(&indicator_show);
        indicator_buttons.append(&indicator_cancel);
        let indicator_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        indicator_box.set_margin_top(12);
        indicator_box.set_margin_bottom(12);
        indicator_box.set_margin_start(12);
        indicator_box.set_margin_end(12);
        indicator_box.set_width_request(300);
        indicator_box.append(&indicator_title);
        indicator_box.append(&indicator_detail);
        indicator_box.append(&indicator_bar);
        indicator_box.append(&indicator_buttons);
        let indicator_popover = gtk::Popover::new();
        indicator_popover.set_child(Some(&indicator_box));
        let transfer_indicator = gtk::MenuButton::new();
        transfer_indicator.set_child(Some(&indicator_ring));
        transfer_indicator.set_popover(Some(&indicator_popover));
        transfer_indicator.add_css_class("flat");
        transfer_indicator.set_tooltip_text(Some("Transfer progress"));
        transfer_indicator.set_visible(false);

        let window_title = adw::WindowTitle::new("LocalSend", "Nearby Devices");
        let header = adw::HeaderBar::new();
        header.pack_start(&sidebar_toggle);
        header.set_title_widget(Some(&window_title));
        header.pack_end(&menu_button);
        header.pack_end(&transfer_indicator);

        let banner = adw::Banner::new("");
        banner.set_revealed(false);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&banner);
        toolbar.set_content(Some(&split));

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&toolbar));
        window.set_content(Some(&toasts));

        Self {
            window,
            core,
            split,
            sidebar_toggle,
            window_title,
            sidebar_stack,
            device_list,
            device_keys: RefCell::new(Vec::new()),
            updating_list: Cell::new(false),
            content_stack,
            device_avatar,
            device_name,
            device_meta,
            home_drop,
            add_files_button,
            add_folder_button,
            queue_text_button,
            queue_list,
            queue_section,
            search_spinner,
            search_status,
            decision_title,
            decision_body,
            decision_accept,
            decision_decline,
            send_button,
            send_folder_button,
            send_text_button,
            drop_zone,
            transfer_title,
            transfer_file,
            transfer_progress,
            transfer_detail,
            transfer_cancel,
            transfer_indicator,
            indicator_ring,
            indicator_fraction,
            indicator_title,
            indicator_detail,
            indicator_bar,
            indicator_show,
            indicator_cancel,
            result_page,
            result_primary,
            result_secondary,
            result_action: RefCell::new(ResultAction::None),
            toasts,
            banner,
            selected: RefCell::new(None),
            transfer: RefCell::new(None),
            device_revision: Cell::new(0),
            queue: RefCell::new(Vec::new()),
            queued_texts: RefCell::new(Vec::new()),
            queue_device: RefCell::new(None),
            background: RefCell::new(false),
            notification_percent: Cell::new(u64::MAX),
            notification_at: Cell::new(None),
            pending_text: RefCell::new(None),
            pending_decision: RefCell::new(None),
            history_dialog: RefCell::new(None),
            history_groups: RefCell::new(Vec::new()),
            history_expanded: RefCell::new(HashSet::new()),
            history_refresh_queued: Cell::new(false),
        }
    }

    fn wire(self: &Rc<Self>, app: &adw::Application) {
        self.send_button.connect_clicked({
            let ui = self.clone();
            move |_| ui.choose_files()
        });
        self.send_folder_button.connect_clicked({
            let ui = self.clone();
            move |_| ui.choose_folder()
        });
        self.send_text_button.connect_clicked({
            let ui = self.clone();
            move |_| ui.send_text_dialog()
        });
        self.add_files_button.connect_clicked({
            let ui = self.clone();
            move |_| ui.queue_files_dialog()
        });
        self.add_folder_button.connect_clicked({
            let ui = self.clone();
            move |_| ui.queue_folder_dialog()
        });
        self.queue_text_button.connect_clicked({
            let ui = self.clone();
            move |_| ui.show_text_dialog(None)
        });
        self.decision_accept.connect_clicked({
            let ui = self.clone();
            move |_| ui.answer_decision(true)
        });
        self.decision_decline.connect_clicked({
            let ui = self.clone();
            move |_| ui.answer_decision(false)
        });
        self.transfer_cancel.connect_clicked({
            let ui = self.clone();
            move |_| ui.cancel_transfer()
        });

        self.indicator_cancel.connect_clicked({
            let ui = self.clone();
            move |_| {
                ui.transfer_indicator.popdown();
                ui.cancel_transfer();
            }
        });
        self.indicator_show.connect_clicked({
            let ui = self.clone();
            move |_| {
                ui.transfer_indicator.popdown();
                ui.sync_transfer_view();
            }
        });

        self.transfer_file.connect_clicked({
            let ui = self.clone();
            move |_| ui.show_receive_files_dialog()
        });
        self.result_primary.connect_clicked({
            let ui = self.clone();
            move |_| ui.result_primary_clicked()
        });
        self.result_secondary.connect_clicked({
            let ui = self.clone();
            move |_| ui.back_to_device()
        });

        self.window.connect_visible_notify({
            let ui = self.clone();
            move |window| {
                crate::tray::set_window_visible(window.is_visible());
                if window.is_visible() {
                    if ui.pending_decision.borrow().is_some() {
                        ui.show_decision();
                    } else {
                        ui.sync_transfer_view();
                    }
                    if ui.window.is_active() {
                        ui.clear_notification();
                    }
                }
            }
        });

        self.device_list.connect_row_selected({
            let ui = self.clone();
            move |_, row| {
                if ui.updating_list.get() {
                    return;
                }
                let Some(row) = row else {
                    return;
                };
                let index = row.index();
                if index < 0 {
                    return;
                }
                let key = ui.device_keys.borrow().get(index as usize).cloned();
                if let Some(key) = key {
                    ui.select_device(&key);
                    if ui.split.property::<bool>("collapsed") {
                        ui.split.set_show_sidebar(false);
                    }
                }
            }
        });

        let _ = self
            .split
            .bind_property("show-sidebar", &self.sidebar_toggle, "active")
            .bidirectional()
            .build();

        self.install_drop_target();
        self.install_actions();
        self.install_accels(app);
        self.install_close_handler();
        self.install_receive_handlers();
        self.refresh_queue();
        self.sync_search_status();
    }

    fn install_close_handler(self: &Rc<Self>) {
        let ui = self.clone();
        self.window.connect_close_request(move |_| {
            // Only hide when a tray icon is actually running, otherwise the
            // window would disappear with no way to bring it back.
            if ui.core.config().run_in_background && ui.core.tray_active() {
                ui.window.set_visible(false);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }

    /// One timer for the whole window: the device list, the sidebar search
    /// status and the background state all share it.
    fn install_receive_handlers(self: &Rc<Self>) {
        let ui = self.clone();
        glib::timeout_add_seconds_local(1, move || {
            ui.sync_background_state();
            if ui.device_revision.get() != ui.core.device_revision() {
                ui.refresh_devices();
            }
            ui.sync_search_status();
            ui.refresh_transfer_indicator();
            glib::ControlFlow::Continue
        });
    }

    /// Cheap: only touches the background flag when the window visibility
    /// changed, so the timer never clones the whole configuration.
    fn sync_background_state(&self) {
        let hidden = !self.window.is_visible();
        if hidden == *self.background.borrow() {
            return;
        }
        *self.background.borrow_mut() = hidden;
        if !hidden {
            self.sync_transfer_view();
        } else {
            self.release_background_caches();
            Self::trim_memory();
            glib::idle_add_local_once(Self::trim_memory);
            glib::timeout_add_seconds_local_once(2, Self::trim_memory);
        }
    }

    fn release_background_caches(&self) {
        // Drop cached dialogs so their widget trees can be freed while hidden.
        if let Some((dialog, _)) = self.history_dialog.borrow_mut().take() {
            dialog.force_close();
        }
    }

    fn trim_memory() {
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        unsafe {
            // Return freed heap pages to the OS when the window is hidden.
            extern "C" {
                fn malloc_trim(pad: usize) -> std::ffi::c_int;
            }
            let _ = malloc_trim(0);
        }
    }

    fn set_background(&self, _enabled: bool) {}

    /// Keeps the transfer page in step with what is actually running, so a
    /// transfer that started in the background is visible as soon as the
    /// window is opened.
    fn sync_transfer_view(&self) {
        self.refresh_transfer_indicator();
        let subtitle = self
            .transfer
            .borrow()
            .as_ref()
            .map(|state| match state.kind {
                TransferKind::Receiving => "Receiving Files",
                TransferKind::Sending => "Sending Files",
            });
        if let Some(subtitle) = subtitle {
            if self.content_stack.visible_child_name().as_deref() != Some("transfer") {
                self.window_title.set_subtitle(subtitle);
                self.content_stack.set_visible_child_name("transfer");
            }
        }
    }

    /// Mirrors the current transfer into the header ring and its popover, so
    /// progress stays reachable after switching away from the transfer page.
    fn refresh_transfer_indicator(&self) {
        let state = self.transfer.borrow();
        let Some(state) = state.as_ref() else {
            self.transfer_indicator.set_visible(false);
            if self.indicator_fraction.get() != 0.0 {
                self.indicator_fraction.set(0.0);
                self.indicator_ring.queue_draw();
            }
            return;
        };
        let fraction = if state.total == 0 {
            0.0
        } else {
            (state.done as f64 / state.total as f64).clamp(0.0, 1.0)
        };
        if (self.indicator_fraction.get() - fraction).abs() > f64::EPSILON {
            self.indicator_fraction.set(fraction);
            self.indicator_ring.queue_draw();
        }
        self.indicator_bar.set_fraction(fraction);
        let (title, detail) = match state.kind {
            TransferKind::Receiving => (
                "Receiving files".to_string(),
                state
                    .receive_label
                    .clone()
                    .unwrap_or_else(|| format!("{} received", util::human_bytes(state.done))),
            ),
            TransferKind::Sending => (
                "Sending files".to_string(),
                format!(
                    "{} of {}",
                    util::human_bytes(state.done),
                    util::human_bytes(state.total)
                ),
            ),
        };
        self.indicator_title.set_label(&title);
        self.indicator_detail.set_label(&detail);
        self.transfer_indicator.set_visible(true);
    }

    fn sync_search_status(&self) {
        let searching = self.device_keys.borrow().is_empty();
        self.search_status.set_visible(searching);
        if searching {
            self.search_spinner.start();
        } else {
            self.search_spinner.stop();
        }
    }

    /// A notification is never shown while the window has focus.
    fn foreground(&self) -> bool {
        self.window.is_visible() && self.window.is_active()
    }

    /// All transfer notifications share one id, so a new transfer replaces the
    /// previous popup instead of stacking another one on top of it.
    fn notify_transfer(&self, title: &str, body: &str, popup: bool) {
        if self.foreground() {
            return;
        }
        // A visible but unfocused window only announces finished transfers.
        if !popup && self.window.is_visible() {
            return;
        }
        let Some(app) = self.window.application() else {
            return;
        };
        let notification = gio::Notification::new(title);
        notification.set_body(Some(body));
        // GNOME shows a replaced notification "as if it was new", so progress
        // updates are sent with low priority: the existing tray entry is
        // updated in place and no banner pops up for every percent.
        notification.set_priority(if popup {
            gio::NotificationPriority::Normal
        } else {
            gio::NotificationPriority::Low
        });
        app.send_notification(Some(NOTIFICATION_ID), &notification);
        self.notification_at.set(Some(Instant::now()));
    }

    /// Progress only matters while the window is hidden; it reuses the same
    /// notification so the user never sees a stale "0%" entry left behind.
    fn notify_progress(&self, title: &str, percent: u64, body: &str) {
        if self.window.is_visible() {
            return;
        }
        if percent == self.notification_percent.get() {
            return;
        }
        if let Some(last) = self.notification_at.get() {
            if percent < 100 && last.elapsed() < Duration::from_millis(350) {
                return;
            }
        }
        self.notification_percent.set(percent);
        self.notify_transfer(title, body, false);
    }

    fn clear_notification(&self) {
        self.notification_percent.set(u64::MAX);
        if let Some(app) = self.window.application() {
            app.withdraw_notification(NOTIFICATION_ID);
        }
    }

    fn install_actions(self: &Rc<Self>) {
        let action = gio::SimpleAction::new("open-files", None);
        action.connect_activate({
            let ui = self.clone();
            move |_, _| ui.choose_files()
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new("open-folder", None);
        action.connect_activate({
            let ui = self.clone();
            move |_, _| ui.choose_folder()
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new("send-text", None);
        action.connect_activate({
            let ui = self.clone();
            move |_, _| ui.send_text_dialog()
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new("history", None);
        action.connect_activate({
            let ui = self.clone();
            move |_, _| ui.show_history()
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new("history-copy-item", Some(glib::VariantTy::STRING));
        action.connect_activate({
            let ui = self.clone();
            move |_, param| {
                if let Some(id) = param.and_then(|value| value.get::<String>()) {
                    ui.history_copy_item(&id);
                }
            }
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new("settings", None);
        action.connect_activate({
            let ui = self.clone();
            move |_, _| ui.show_settings()
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new("shortcuts", None);
        action.connect_activate({
            let ui = self.clone();
            move |_, _| ui.show_shortcuts()
        });
        self.window.add_action(&action);

        let action = gio::SimpleAction::new("about", None);
        action.connect_activate({
            let ui = self.clone();
            move |_, _| ui.show_about()
        });
        self.window.add_action(&action);
    }

    fn install_accels(&self, app: &adw::Application) {
        app.set_accels_for_action("win.open-files", &["<Control>o"]);
        app.set_accels_for_action("win.open-folder", &["<Control><Shift>o"]);
        app.set_accels_for_action("win.send-text", &["<Control><Shift>t"]);
        app.set_accels_for_action("win.history", &["<Control>h"]);
        app.set_accels_for_action("win.settings", &["<Control>comma"]);
        app.set_accels_for_action("win.shortcuts", &["<Control>question", "F1"]);
    }

    fn install_drop_target(self: &Rc<Self>) {
        let drop_target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        drop_target.connect_enter({
            let zone = self.drop_zone.clone();
            let home = self.home_drop.clone();
            move |_, _, _| {
                zone.add_css_class("drag-over");
                home.add_css_class("drag-over");
                gdk::DragAction::COPY
            }
        });
        drop_target.connect_leave({
            let zone = self.drop_zone.clone();
            let home = self.home_drop.clone();
            move |_| {
                zone.remove_css_class("drag-over");
                home.remove_css_class("drag-over");
            }
        });
        drop_target.connect_drop({
            let ui = self.clone();
            move |_, value, _, _| {
                let Ok(file_list) = value.get::<gdk::FileList>() else {
                    return false;
                };
                let paths: Vec<PathBuf> = file_list
                    .files()
                    .iter()
                    .filter_map(|file| file.path())
                    .collect();
                if paths.is_empty() {
                    return false;
                }
                ui.handle_dropped_files(paths);
                true
            }
        });
        self.window.add_controller(drop_target);
    }

    fn spawn_event_loop(self: &Rc<Self>, receiver: async_channel::Receiver<Event>) {
        let ui = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(event) = receiver.recv().await {
                ui.handle_event(event);
            }
        });
    }

    fn handle_event(self: &Rc<Self>, event: Event) {
        match event {
            Event::DevicesChanged => self.refresh_devices(),
            Event::ServerError(message) => {
                self.banner.set_title(&message);
                self.banner.set_revealed(true);
            }
            Event::IncomingRequest(request) => self.present_incoming(request),
            Event::ReceiveProgress {
                session_id,
                file_name,
                done,
                total,
                files_done,
                files_total,
            } => {
                if !self.is_receiving(&session_id) {
                    return;
                }
                if let Some(state) = self.transfer.borrow_mut().as_mut() {
                    state.done = done;
                    state.total = total;
                }
                self.update_progress(done, total, &file_name, files_done, files_total);
                let percent = progress_percent(done, total);
                let progress_name = if files_total > 1 {
                    format!("{files_total} files")
                } else {
                    file_name.clone()
                };
                self.notify_progress(
                    "Receiving",
                    percent,
                    &format!("{percent}% · {progress_name} ({files_done}/{files_total})"),
                );
            }
            Event::ReceiveFinished {
                session_id,
                alias,
                directory,
                files,
                messages,
                total_bytes,
            } => {
                if !self.is_receiving(&session_id) {
                    return;
                }
                self.transfer.borrow_mut().take();
                if files.is_empty() && !messages.is_empty() {
                    if self.window.is_visible() {
                        self.show_text_messages(&alias, messages, true);
                    }
                    self.toast(&format!("Message received from {alias}"));
                    self.notify_transfer(
                        &format!("Message from {alias}"),
                        "Copied to the clipboard",
                        true,
                    );
                    return;
                }
                if files.is_empty() {
                    self.back_to_device();
                    return;
                }
                if !messages.is_empty() {
                    self.show_text_messages(&alias, messages, false);
                }
                let description = format!(
                    "{} file{} ({}) received from {alias}.",
                    files.len(),
                    plural(files.len()),
                    util::human_bytes(total_bytes)
                );
                self.show_result(
                    "emblem-ok-symbolic",
                    "Files Received",
                    &description,
                    "Open Folder",
                    ResultAction::Received {
                        directory: directory.clone(),
                    },
                );
                self.toast(&format!("Received files from {alias}"));
                self.notify_transfer(&format!("Received from {alias}"), &description, true);
            }
            Event::ReceiveText { alias, messages } => {
                let receiving = self
                    .transfer
                    .borrow()
                    .as_ref()
                    .is_some_and(|state| matches!(&state.kind, TransferKind::Receiving));
                if receiving {
                    self.transfer.borrow_mut().take();
                }
                self.notification_percent.set(u64::MAX);
                let body = messages.join("\n\n");
                self.copy_text(&body);
                self.pending_text.borrow_mut().replace(PendingText {
                    alias: alias.clone(),
                    messages: messages.clone(),
                    received: Instant::now(),
                });
                if self.window.is_visible() {
                    if self.transfer.borrow().is_none() {
                        self.back_to_device();
                    }
                    self.toast(&format!("Message from {alias} copied to the clipboard"));
                    // Show the message in the window when it is visible, instead of
                    // leaving it only in the clipboard/notification.
                    self.show_text_messages(&alias, messages, false);
                    return;
                }
                if let Some(app) = self.window.application() {
                    let notification = gio::Notification::new(&format!("Message from {alias}"));
                    notification.set_body(Some(&summarize(&body)));
                    notification.set_default_action("app.show-pending-text");
                    app.send_notification(Some(NOTIFICATION_ID), &notification);
                }
            }
            Event::IncomingWithdrawn { session_id } => {
                let current = self
                    .pending_decision
                    .borrow()
                    .as_ref()
                    .map(|pending| pending.session_id.clone());
                if current.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                self.pending_decision.borrow_mut().take();
                if let Some(app) = self.window.application() {
                    app.withdraw_notification(NOTIFICATION_ID);
                }
                self.toast("The sender is no longer waiting");
                if self.content_stack.visible_child_name().as_deref() == Some("decision") {
                    self.back_to_device();
                }
            }
            Event::ReceiveFailed {
                session_id,
                alias,
                message,
            } => {
                if !self.is_receiving(&session_id) {
                    return;
                }
                self.transfer.borrow_mut().take();
                let description = format!("The transfer from {alias} failed: {message}");
                self.show_result(
                    "dialog-warning-symbolic",
                    "Transfer Failed",
                    &description,
                    "Back",
                    ResultAction::Back,
                );
                self.notify_transfer("Transfer failed", &description, true);
            }
            Event::ReceiveCanceled { session_id, alias } => {
                if !self.is_receiving(&session_id) {
                    return;
                }
                self.transfer.borrow_mut().take();
                self.toast(&format!("{alias} canceled the transfer"));
                self.notify_transfer(
                    "Transfer canceled",
                    &format!("{alias} canceled the transfer"),
                    true,
                );
                self.back_to_device();
            }
            Event::SendProgress {
                done,
                total,
                file_name,
                files_done,
                files_total,
                ..
            } => {
                if self.transfer.borrow().is_none() {
                    return;
                }
                if let Some(state) = self.transfer.borrow_mut().as_mut() {
                    state.done = done;
                    state.total = total;
                }
                self.update_progress(done, total, &file_name, files_done, files_total);
                let percent = progress_percent(done, total);
                // Several uploads run at once, so the file that reported last
                // is not "the" current file. Name the batch instead of letting
                // the label flicker between files.
                let progress_name = if files_total > 1 {
                    format!("{files_total} files")
                } else {
                    file_name.clone()
                };
                self.notify_progress(
                    "Sending",
                    percent,
                    &format!("{percent}% · {progress_name} ({files_done}/{files_total})"),
                );
            }
            Event::SendFinished {
                device,
                files,
                total_bytes,
            } => {
                if self.transfer.borrow().is_none() {
                    return;
                }
                self.transfer.borrow_mut().take();
                let alias = self
                    .core
                    .device(&device)
                    .map(|(info, _)| info.alias)
                    .unwrap_or_else(|| "the device".to_string());
                let description = if files == 0 {
                    format!("{alias} did not need any files.")
                } else {
                    format!(
                        "{} file{} ({}) sent to {alias}.",
                        files,
                        plural(files),
                        util::human_bytes(total_bytes)
                    )
                };
                self.show_result(
                    "emblem-ok-symbolic",
                    "Files Sent",
                    &description,
                    "Send More",
                    ResultAction::Sent {
                        device: device.clone(),
                    },
                );
                self.toast(&format!("Files sent to {alias}"));
                self.notify_transfer(&format!("Sent to {alias}"), &description, true);
                self.advance_queue();
            }
            Event::SendTextFinished { device } => {
                if self.transfer.borrow().is_none() {
                    return;
                }
                self.transfer.borrow_mut().take();
                let alias = self
                    .core
                    .device(&device)
                    .map(|(info, _)| info.alias)
                    .unwrap_or_else(|| "the device".to_string());
                self.show_result(
                    "emblem-ok-symbolic",
                    "Message Sent",
                    &format!("Message sent to {alias}."),
                    "Send Another",
                    ResultAction::MessageSent {
                        device: device.clone(),
                    },
                );
                self.toast(&format!("Message sent to {alias}"));
                self.notify_transfer(&format!("Sent to {alias}"), "Message delivered", true);
                self.advance_queue();
            }
            Event::SendDeclined { device } => {
                if self.transfer.borrow().is_none() {
                    return;
                }
                self.transfer.borrow_mut().take();
                let alias = self
                    .core
                    .device(&device)
                    .map(|(info, _)| info.alias)
                    .unwrap_or_else(|| "The device".to_string());
                self.show_result(
                    "dialog-warning-symbolic",
                    "Transfer Declined",
                    &format!("{alias} declined the transfer. Nothing was sent."),
                    "Back",
                    ResultAction::Back,
                );
                self.toast("The transfer was declined");
                self.queued_texts.borrow_mut().clear();
                self.queue_device.borrow_mut().take();
            }
            Event::SendUnavailable { message, .. } => {
                if self.transfer.borrow().is_none() {
                    return;
                }
                self.transfer.borrow_mut().take();
                self.show_result(
                    "network-offline-symbolic",
                    "Device Unavailable",
                    &format!("{message}. It was removed from the device list."),
                    "Back",
                    ResultAction::Back,
                );
                self.toast(&message);
                self.queued_texts.borrow_mut().clear();
                self.queue_device.borrow_mut().take();
            }
            Event::SendFailed { device, message } => {
                if self.transfer.borrow().is_none() {
                    return;
                }
                self.transfer.borrow_mut().take();
                let description = match self.core.device(&device) {
                    Some((info, _)) => {
                        format!("The transfer with {} failed: {message}", info.alias)
                    }
                    None => message,
                };
                self.show_result(
                    "dialog-warning-symbolic",
                    "Transfer Failed",
                    &description,
                    "Back",
                    ResultAction::Back,
                );
                self.notify_transfer("Transfer failed", &description, true);
                self.queued_texts.borrow_mut().clear();
                self.queue_device.borrow_mut().take();
            }
            Event::SendNeedsPin { device, paths } => {
                self.transfer.borrow_mut().take();
                self.show_pin_dialog(device, paths, None);
            }
            Event::SendTextNeedsPin { device, text } => {
                self.transfer.borrow_mut().take();
                self.show_pin_dialog(device, Vec::new(), Some(text));
            }
            Event::SendCanceled => {}
            Event::TrayShow => self.window.present(),
            Event::TrayHide => self.window.set_visible(false),
            Event::TrayQuit => {
                if let Some(app) = self.window.application() {
                    app.quit();
                }
            }
            Event::HistoryChanged => self.refresh_history_dialog(),
            Event::Toast(message) => self.toast(&message),
        }
    }

    fn refresh_devices(&self) {
        let devices = self.core.devices_sorted();
        self.updating_list.set(true);

        while let Some(child) = self.device_list.first_child() {
            self.device_list.remove(&child);
        }

        let mut keys = Vec::with_capacity(devices.len());
        for device in &devices {
            let row = adw::ActionRow::builder()
                .title(&device.alias)
                .subtitle(format!(
                    "{} · {}",
                    util::device_type_label(&device.device_type),
                    device.ip
                ))
                .activatable(true)
                .build();
            let avatar = adw::Avatar::new(40, Some(&util::initials(&device.alias)), true);
            avatar.set_valign(gtk::Align::Center);
            row.add_prefix(&avatar);
            let icon = gtk::Image::from_icon_name(util::device_type_icon(&device.device_type));
            icon.add_css_class("dim-label");
            row.add_suffix(&icon);
            self.device_list.append(&row);
            keys.push(device.key.clone());
        }
        self.device_keys.replace(keys);
        self.sidebar_stack
            .set_visible_child_name(if devices.is_empty() { "empty" } else { "list" });

        let selected = self.selected.borrow().clone();
        if let Some(selected) = selected {
            let index = self
                .device_keys
                .borrow()
                .iter()
                .position(|key| key == &selected);
            match index {
                Some(index) => {
                    if let Some(row) = self.device_list.row_at_index(index as i32) {
                        self.device_list.select_row(Some(&row));
                    }
                }
                None => {
                    // The selected device went away: forget it and leave its
                    // now-dead page, showing a running transfer or the home
                    // page instead.
                    self.selected.borrow_mut().take();
                    let on_device_page = matches!(
                        self.content_stack.visible_child_name().as_deref(),
                        Some("device") | Some("result")
                    );
                    if self.transfer.borrow().is_some() {
                        if on_device_page {
                            self.sync_transfer_view();
                        }
                    } else if on_device_page {
                        if !self.queue.borrow().is_empty() {
                            self.toast("The device is gone. Your queue was kept.");
                        }
                        self.show_empty();
                    }
                }
            }
        }

        self.updating_list.set(false);
        self.device_revision.set(self.core.device_revision());
    }

    fn show_empty(&self) {
        self.content_stack.set_visible_child_name("home");
        self.window_title.set_subtitle("Nearby Devices");
    }

    /// Selecting a device with a non-empty queue asks where the queue should
    /// go before anything is sent.
    fn select_device(self: &Rc<Self>, key: &str) {
        // Switching devices never interrupts a transfer: its progress stays
        // available in the header indicator and its popover.
        self.show_device(key);
        if self.queue.borrow().is_empty() || self.transfer.borrow().is_some() {
            return;
        }
        self.offer_queue(key);
    }

    fn offer_queue(self: &Rc<Self>, key: &str) {
        let Some((info, _)) = self.core.device(key) else {
            return;
        };
        let (files, texts) = self.queue_summary();
        let mut parts = Vec::new();
        if files > 0 {
            parts.push(format!("{files} file item{}", plural(files)));
        }
        if texts > 0 {
            parts.push(format!("{texts} message{}", plural(texts)));
        }
        let dialog = adw::AlertDialog::builder()
            .heading(&format!("Send to {}?", info.alias))
            .body(&format!("{}, queued so far.", parts.join(" and ")))
            .default_response("cancel")
            .close_response("cancel")
            .build();
        dialog.add_response("cancel", "Later");
        dialog.add_response("send", "Send Now");
        dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
        let ui = self.clone();
        let device = key.to_string();
        dialog.choose(
            Some(&self.window),
            gio::Cancellable::NONE,
            move |response| {
                if response.as_str() == "send" {
                    ui.flush_queue(device.clone());
                }
            },
        );
    }

    fn queue_summary(&self) -> (usize, usize) {
        let mut files = 0;
        let mut texts = 0;
        for item in self.queue.borrow().iter() {
            match item {
                QueueItem::Files { .. } => files += 1,
                QueueItem::Text { .. } => texts += 1,
            }
        }
        (files, texts)
    }

    fn queue_files(self: &Rc<Self>, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        self.queue.borrow_mut().push(QueueItem::files(paths));
        self.refresh_queue();
        self.show_empty();
        self.toast(&format!(
            "Queued {count} item{}. Pick a device to send.",
            plural(count)
        ));
    }

    fn queue_text(self: &Rc<Self>, text: String) {
        self.queue.borrow_mut().push(QueueItem::text(text));
        self.refresh_queue();
        self.show_empty();
        self.toast("Message queued. Pick a device to send.");
    }

    fn refresh_queue(self: &Rc<Self>) {
        while let Some(child) = self.queue_list.first_child() {
            self.queue_list.remove(&child);
        }
        let queue_len = self.queue.borrow().len();
        for index in 0..queue_len {
            let item = self.queue.borrow()[index].clone();
            match item {
                QueueItem::Files {
                    paths,
                    label,
                    detail,
                } => {
                    let icon_name = queue_icon_for_paths(&paths);
                    let remove = gtk::Button::from_icon_name("window-close-symbolic");
                    remove.add_css_class("flat");
                    remove.set_valign(gtk::Align::Center);
                    remove.set_tooltip_text(Some("Remove from the queue"));
                    let ui = self.clone();
                    remove.connect_clicked(move |_| {
                        if index < ui.queue.borrow().len() {
                            ui.queue.borrow_mut().remove(index);
                        }
                        ui.refresh_queue();
                    });

                    if paths.len() > 1 {
                        let row = adw::ExpanderRow::builder()
                            .title(&label)
                            .subtitle(&detail)
                            .build();
                        row.set_title_lines(1);
                        row.set_subtitle_lines(1);
                        let icon = gtk::Image::from_icon_name(icon_name);
                        icon.add_css_class("dim-label");
                        row.add_prefix(&icon);
                        row.add_suffix(&remove);

                        for (file_index, path) in paths.iter().enumerate() {
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| path.to_string_lossy().to_string());
                            let subtitle = match std::fs::metadata(path) {
                                Ok(meta) if meta.is_file() => util::human_bytes(meta.len()),
                                Ok(_) => "Folder".to_string(),
                                Err(_) => "Unavailable".to_string(),
                            };
                            let child = adw::ActionRow::builder()
                                .title(&name)
                                .subtitle(&subtitle)
                                .build();
                            child.set_title_lines(1);
                            child.set_subtitle_lines(1);
                            let child_icon = gtk::Image::from_icon_name(&icon_for_path(path));
                            child_icon.add_css_class("dim-label");
                            child.add_prefix(&child_icon);

                            let child_remove = gtk::Button::from_icon_name("window-close-symbolic");
                            child_remove.add_css_class("flat");
                            child_remove.set_valign(gtk::Align::Center);
                            child_remove.set_tooltip_text(Some("Remove this file"));
                            let ui = self.clone();
                            child_remove.connect_clicked(move |_| {
                                let mut queue = ui.queue.borrow_mut();
                                if index >= queue.len() {
                                    drop(queue);
                                    ui.refresh_queue();
                                    return;
                                }
                                let remove_group = match &mut queue[index] {
                                    QueueItem::Files { paths, .. } => {
                                        if file_index < paths.len() {
                                            paths.remove(file_index);
                                        }
                                        if paths.is_empty() {
                                            true
                                        } else if paths.len() == 1 {
                                            let remaining = std::mem::take(paths);
                                            queue[index] = QueueItem::files(remaining);
                                            false
                                        } else {
                                            let remaining = std::mem::take(paths);
                                            queue[index] = QueueItem::files(remaining);
                                            false
                                        }
                                    }
                                    _ => false,
                                };
                                if remove_group {
                                    queue.remove(index);
                                }
                                drop(queue);
                                ui.refresh_queue();
                            });
                            child.add_suffix(&child_remove);
                            row.add_row(&child);
                        }
                        self.queue_list.append(&row);
                    } else {
                        let row = adw::ActionRow::builder()
                            .title(&label)
                            .subtitle(&detail)
                            .build();
                        row.set_title_lines(1);
                        row.set_subtitle_lines(1);
                        let icon = gtk::Image::from_icon_name(icon_name);
                        icon.add_css_class("dim-label");
                        row.add_prefix(&icon);
                        row.add_suffix(&remove);
                        self.queue_list.append(&row);
                    }
                }
                QueueItem::Text { label, detail, .. } => {
                    let row = adw::ActionRow::builder()
                        .title(&label)
                        .subtitle(&detail)
                        .build();
                    row.set_title_lines(1);
                    row.set_subtitle_lines(1);
                    let icon = gtk::Image::from_icon_name("chat-message-new-symbolic");
                    icon.add_css_class("dim-label");
                    row.add_prefix(&icon);
                    let remove = gtk::Button::from_icon_name("window-close-symbolic");
                    remove.add_css_class("flat");
                    remove.set_valign(gtk::Align::Center);
                    remove.set_tooltip_text(Some("Remove from the queue"));
                    let ui = self.clone();
                    remove.connect_clicked(move |_| {
                        if index < ui.queue.borrow().len() {
                            ui.queue.borrow_mut().remove(index);
                        }
                        ui.refresh_queue();
                    });
                    row.add_suffix(&remove);
                    self.queue_list.append(&row);
                }
            }
        }
        self.queue_section
            .set_visible(!self.queue.borrow().is_empty());
    }

    /// Sends everything in the queue to one device: files first, then the
    /// queued messages one after another.
    fn flush_queue(self: &Rc<Self>, device: String) {
        if self.transfer.borrow().is_some() {
            self.toast("A transfer is already in progress");
            return;
        }
        if self.core.device(&device).is_none() {
            self.toast("The device is no longer available");
            return;
        }
        let items = std::mem::take(&mut *self.queue.borrow_mut());
        let mut paths = Vec::new();
        let mut texts = Vec::new();
        for item in items {
            match item {
                QueueItem::Files {
                    paths: mut item_paths,
                    ..
                } => paths.append(&mut item_paths),
                QueueItem::Text { text, .. } => texts.push(text),
            }
        }
        self.refresh_queue();
        *self.queued_texts.borrow_mut() = texts;
        self.queue_device.replace(Some(device.clone()));
        *self.selected.borrow_mut() = Some(device.clone());
        if !paths.is_empty() {
            self.start_send(device, paths, None);
            return;
        }
        match self.next_queued_text() {
            Some(text) => self.start_send_text(device, text, None),
            None => {
                self.queue_device.borrow_mut().take();
            }
        }
    }

    /// Continues a queued send after a transfer finished.
    fn advance_queue(self: &Rc<Self>) {
        let Some(device) = self.queue_device.borrow().clone() else {
            return;
        };
        if self.core.device(&device).is_none() {
            self.queued_texts.borrow_mut().clear();
            self.queue_device.borrow_mut().take();
            return;
        }
        match self.next_queued_text() {
            Some(text) => self.start_send_text(device, text, None),
            None => {
                self.queue_device.borrow_mut().take();
            }
        }
    }

    fn next_queued_text(&self) -> Option<String> {
        let mut texts = self.queued_texts.borrow_mut();
        if texts.is_empty() {
            None
        } else {
            Some(texts.remove(0))
        }
    }

    fn show_device(&self, key: &str) {
        let Some((info, ip)) = self.core.device(key) else {
            self.show_empty();
            return;
        };
        *self.selected.borrow_mut() = Some(key.to_string());
        self.device_avatar
            .set_text(Some(&util::initials(&info.alias)));
        self.device_name.set_label(&info.alias);
        self.device_meta.set_label(&format!(
            "{} · {}",
            util::device_type_label(info.device_type()),
            ip
        ));
        self.content_stack.set_visible_child_name("device");
        self.window_title.set_subtitle(&info.alias);
    }

    fn back_to_device(&self) {
        // Drop the RefCell borrow before show_device() mutates `selected`.
        // Keeping it alive here panicked when Done was pressed after a receive.
        let selected = self.selected.borrow().clone();
        if let Some(key) = selected {
            if self.core.device(&key).is_some() {
                self.show_device(&key);
                return;
            }
        }
        self.show_empty();
    }
    fn show_receive_files_dialog(self: &Rc<Self>) {
        let session_id = self.transfer.borrow().as_ref().and_then(|state| {
            if matches!(state.kind, TransferKind::Receiving) {
                state.session_id.clone()
            } else {
                None
            }
        });
        let Some(session_id) = session_id else {
            return;
        };
        let Some(names) = self.core.receive_file_names(&session_id) else {
            return;
        };
        if names.len() <= 1 {
            return;
        }

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.set_valign(gtk::Align::Start);
        let rows: Rc<RefCell<Vec<(String, gtk::Label)>>> = Rc::new(RefCell::new(Vec::new()));
        for name in &names {
            let label = gtk::Label::builder()
                .label(name)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .selectable(true)
                .build();
            let status = gtk::Label::new(Some("Waiting"));
            status.add_css_class("caption");
            status.add_css_class("dim-label");
            status.set_valign(gtk::Align::Center);
            let row_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row_box.set_margin_top(10);
            row_box.set_margin_bottom(10);
            row_box.set_margin_start(12);
            row_box.set_margin_end(12);
            row_box.append(&label);
            row_box.append(&status);
            let row = gtk::ListBoxRow::new();
            row.set_activatable(false);
            row.set_child(Some(&row_box));
            list.append(&row);
            rows.borrow_mut().push((name.clone(), status));
        }

        let apply_states = |rows: &RefCell<Vec<(String, gtk::Label)>>,
                            states: &[(String, bool)]| {
            for (name, status) in rows.borrow().iter() {
                if let Some((_, done)) = states.iter().find(|(known, _)| known == name) {
                    status.set_label(if *done { "Received" } else { "Waiting" });
                }
            }
        };
        if let Some(states) = self.core.receive_file_done(&session_id) {
            apply_states(&rows, &states);
        }

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&list)
            .build();
        scroller.add_css_class("file-list-scroller");
        scroller.set_overflow(gtk::Overflow::Hidden);

        let clamp = adw::Clamp::new();
        clamp.set_maximum_size(540);
        clamp.set_tightening_threshold(420);
        clamp.set_margin_top(18);
        clamp.set_margin_bottom(18);
        clamp.set_margin_start(18);
        clamp.set_margin_end(18);
        clamp.set_child(Some(&scroller));

        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&adw::WindowTitle::new(
            &format!("{} files", names.len()),
            "In this transfer",
        )));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&clamp));

        let dialog = adw::Dialog::new();
        dialog.set_content_width(600);
        dialog.set_content_height(520);
        dialog.set_child(Some(&toolbar));

        // Keep the per-file state live while the dialog stays open.
        let closed = Rc::new(Cell::new(false));
        dialog.connect_closed({
            let closed = closed.clone();
            move |_| closed.set(true)
        });
        let core = self.core.clone();
        let timer_session = session_id.clone();
        let timer_rows = rows.clone();
        glib::timeout_add_seconds_local(1, move || {
            if closed.get() {
                return glib::ControlFlow::Break;
            }
            if let Some(states) = core.receive_file_done(&timer_session) {
                for (name, status) in timer_rows.borrow().iter() {
                    if let Some((_, done)) = states.iter().find(|(known, _)| known == name) {
                        status.set_label(if *done { "Received" } else { "Waiting" });
                    }
                }
            }
            glib::ControlFlow::Continue
        });

        dialog.present(Some(&self.window));
    }

    fn update_progress(
        &self,
        done: u64,
        total: u64,
        file_name: &str,
        files_done: usize,
        files_total: usize,
    ) {
        let fraction = if total == 0 {
            0.0
        } else {
            (done as f64 / total as f64).clamp(0.0, 1.0)
        };
        self.transfer_progress.set_fraction(fraction);
        let label = {
            let state = self.transfer.borrow();
            if let Some(state) = state.as_ref() {
                if matches!(state.kind, TransferKind::Receiving) {
                    if files_total > 1 {
                        state
                            .receive_label
                            .clone()
                            .unwrap_or_else(|| format!("{files_total} files"))
                    } else {
                        file_name.to_string()
                    }
                } else if files_total > 1 {
                    format!("{files_total} files")
                } else {
                    file_name.to_string()
                }
            } else {
                file_name.to_string()
            }
        };
        self.transfer_file.set_label(&label);
        self.transfer_detail.set_label(&format!(
            "{} of {} · {} of {} files",
            util::human_bytes(done),
            util::human_bytes(total),
            files_done,
            files_total
        ));
        self.refresh_transfer_indicator();
    }

    fn show_result(
        &self,
        icon: &str,
        title: &str,
        description: &str,
        primary_label: &str,
        action: ResultAction,
    ) {
        self.result_page.set_icon_name(Some(icon));
        self.result_page.set_title(title);
        self.result_page.set_description(Some(description));
        self.result_primary.set_label(primary_label);
        self.result_primary.set_visible(!primary_label.is_empty());
        self.result_secondary.set_visible(!primary_label.is_empty());
        *self.result_action.borrow_mut() = action;
        self.content_stack.set_visible_child_name("result");
        self.window_title.set_subtitle(title);
    }

    fn result_primary_clicked(self: &Rc<Self>) {
        let action = std::mem::replace(&mut *self.result_action.borrow_mut(), ResultAction::None);
        match action {
            ResultAction::Received { directory } => open_folder(&directory),
            ResultAction::Sent { device } => {
                if self.core.device(&device).is_some() {
                    *self.selected.borrow_mut() = Some(device);
                    self.choose_files();
                } else {
                    self.show_empty();
                }
            }
            ResultAction::MessageSent { device } => {
                if self.core.device(&device).is_some() {
                    *self.selected.borrow_mut() = Some(device.clone());
                    self.send_text_dialog_for(device);
                } else {
                    self.show_empty();
                }
            }
            ResultAction::Back | ResultAction::None => self.back_to_device(),
        }
    }

    fn choose_files(self: &Rc<Self>) {
        if self.transfer.borrow().is_some() {
            self.toast("A transfer is already in progress");
            return;
        }
        let Some(device) = self.selected.borrow().clone() else {
            self.toast("Select a device first");
            return;
        };
        let dialog = gtk::FileDialog::builder()
            .title("Select Files to Send")
            .modal(true)
            .build();
        let ui = self.clone();
        glib::spawn_future_local(async move {
            match dialog.open_multiple_future(Some(&ui.window)).await {
                Ok(files) => {
                    let mut paths = Vec::new();
                    for index in 0..files.n_items() {
                        if let Some(file) = files.item(index).and_downcast::<gio::File>() {
                            if let Some(path) = file.path() {
                                paths.push(path);
                            }
                        }
                    }
                    if !paths.is_empty() {
                        ui.start_send(device, paths, None);
                    }
                }
                Err(error) => eprintln!("File chooser closed: {error}"),
            }
        });
    }

    fn choose_folder(self: &Rc<Self>) {
        if self.transfer.borrow().is_some() {
            self.toast("A transfer is already in progress");
            return;
        }
        let Some(device) = self.selected.borrow().clone() else {
            self.toast("Select a device first");
            return;
        };
        let dialog = gtk::FileDialog::builder()
            .title("Select a Folder to Send")
            .modal(true)
            .build();
        let ui = self.clone();
        glib::spawn_future_local(async move {
            match dialog.select_folder_future(Some(&ui.window)).await {
                Ok(folder) => {
                    if let Some(path) = folder.path() {
                        ui.start_send(device, vec![path], None);
                    }
                }
                Err(error) => eprintln!("Folder chooser closed: {error}"),
            }
        });
    }

    fn handle_dropped_files(self: &Rc<Self>, paths: Vec<PathBuf>) {
        if self.transfer.borrow().is_some() {
            self.toast("A transfer is already in progress");
            return;
        }
        // A drop with no device selected joins the queue, so files can be
        // collected before anything is available to send them to.
        let Some(device) = self.selected.borrow().clone() else {
            self.queue_files(paths);
            return;
        };
        if self.core.device(&device).is_none() {
            self.selected.borrow_mut().take();
            self.queue_files(paths);
            return;
        }
        self.start_send(device, paths, None);
    }

    fn queue_files_dialog(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Add Files to the Queue")
            .modal(true)
            .build();
        let ui = self.clone();
        glib::spawn_future_local(async move {
            match dialog.open_multiple_future(Some(&ui.window)).await {
                Ok(files) => {
                    let mut paths = Vec::new();
                    for index in 0..files.n_items() {
                        if let Some(file) = files.item(index).and_downcast::<gio::File>() {
                            if let Some(path) = file.path() {
                                paths.push(path);
                            }
                        }
                    }
                    ui.queue_files(paths);
                }
                Err(error) => eprintln!("File chooser closed: {error}"),
            }
        });
    }

    fn queue_folder_dialog(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Add a Folder to the Queue")
            .modal(true)
            .build();
        let ui = self.clone();
        glib::spawn_future_local(async move {
            match dialog.select_folder_future(Some(&ui.window)).await {
                Ok(folder) => {
                    if let Some(path) = folder.path() {
                        ui.queue_files(vec![path]);
                    }
                }
                Err(error) => eprintln!("Folder chooser closed: {error}"),
            }
        });
    }

    fn send_text_dialog(self: &Rc<Self>) {
        if self.transfer.borrow().is_some() {
            self.toast("A transfer is already in progress");
            return;
        }
        let Some(device) = self.selected.borrow().clone() else {
            self.toast("Select a device first");
            return;
        };
        self.send_text_dialog_for(device);
    }

    fn send_text_dialog_for(self: &Rc<Self>, device: String) {
        self.show_text_dialog(Some(device));
    }

    /// `None` queues the message instead of sending it right away.
    fn show_text_dialog(self: &Rc<Self>, device: Option<String>) {
        let target = match device.as_deref() {
            Some(device) => match self.core.device(device) {
                Some((info, _)) => info.alias,
                None => {
                    self.toast("The device is no longer available");
                    return;
                }
            },
            None => String::new(),
        };
        let view = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(8)
            .right_margin(8)
            .build();
        let buffer = view.buffer();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .overlay_scrolling(false)
            .min_content_height(130)
            .max_content_height(280)
            .propagate_natural_height(true)
            .hexpand(true)
            .child(&view)
            .build();
        scroller.add_css_class("text-area");

        let paste_button = gtk::Button::with_label("Paste");
        paste_button.add_css_class("pill");
        paste_button.add_css_class("osd");
        paste_button.set_halign(gtk::Align::End);
        paste_button.set_valign(gtk::Align::End);
        paste_button.set_margin_end(10);
        paste_button.set_margin_bottom(10);
        paste_button.set_tooltip_text(Some("Paste from the clipboard"));
        let ui = self.clone();
        paste_button.connect_clicked({
            let buffer = buffer.clone();
            let ui = ui.clone();
            move |_| {
                let Some(display) = gdk::Display::default() else {
                    return;
                };
                let clipboard = display.clipboard();
                let buffer = buffer.clone();
                let ui = ui.clone();
                glib::spawn_future_local(async move {
                    match clipboard.read_text_future().await {
                        Ok(Some(text)) if !text.is_empty() => buffer.set_text(text.as_str()),
                        Ok(_) => ui.toast("The clipboard is empty"),
                        Err(_) => ui.toast("Could not read the clipboard"),
                    }
                });
            }
        });

        let extra = gtk::Overlay::new();
        extra.set_child(Some(&scroller));
        extra.add_overlay(&paste_button);
        let dialog = adw::AlertDialog::builder()
            .heading(if device.is_some() {
                "Send Text"
            } else {
                "Queue a Message"
            })
            .body(if device.is_some() {
                format!("Send a message to {target}. It is not saved as a file.")
            } else {
                "The message joins the queue and is sent once you pick a device.".to_string()
            })
            .extra_child(&extra)
            .default_response("send")
            .close_response("cancel")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("send", "Send");
        dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
        dialog.set_response_appearance("cancel", adw::ResponseAppearance::Destructive);

        let ui = self.clone();
        let buffer_for_send = buffer.clone();
        dialog.choose(
            Some(&self.window),
            gio::Cancellable::NONE,
            move |response| {
                if response.as_str() != "send" {
                    return;
                }
                let text = buffer_for_send
                    .text(
                        &buffer_for_send.start_iter(),
                        &buffer_for_send.end_iter(),
                        true,
                    )
                    .trim()
                    .to_string();
                if text.is_empty() {
                    ui.toast("Type or paste a message first");
                    return;
                }
                match device {
                    Some(device) => ui.start_send_text(device, text, None),
                    None => ui.queue_text(text),
                }
            },
        );
    }

    fn start_send_text(self: &Rc<Self>, device_key: String, text: String, pin: Option<String>) {
        if self.transfer.borrow().is_some() {
            self.toast("A transfer is already in progress");
            return;
        }
        let Some((info, _)) = self.core.device(&device_key) else {
            self.toast("The device is no longer available");
            return;
        };
        if text.trim().is_empty() {
            self.toast("The message is empty");
            return;
        }
        self.selected.replace(Some(device_key.clone()));
        let cancel = Arc::new(AtomicBool::new(false));
        *self.transfer.borrow_mut() = Some(TransferState {
            kind: TransferKind::Sending,
            cancel: cancel.clone(),
            session_id: None,
            total: text.len() as u64,
            done: 0,
            receive_label: None,
        });
        self.transfer_title
            .set_label(&format!("Sending message to {}", info.alias));
        self.transfer_file.set_label("Text message");
        self.transfer_progress.set_fraction(0.0);
        self.transfer_detail.set_label("Connecting to the device…");
        self.transfer_cancel.set_sensitive(true);
        self.transfer_cancel.set_label("Cancel");
        self.content_stack.set_visible_child_name("transfer");
        self.window_title.set_subtitle("Sending Message");
        self.refresh_transfer_indicator();
        sender::spawn_text(self.core.clone(), device_key, text, pin, cancel);
    }

    fn start_send(self: &Rc<Self>, device_key: String, paths: Vec<PathBuf>, pin: Option<String>) {
        if self.transfer.borrow().is_some() {
            self.toast("A transfer is already in progress");
            return;
        }
        let Some((info, _)) = self.core.device(&device_key) else {
            self.toast("The device is no longer available");
            return;
        };
        if paths.is_empty() {
            return;
        }

        self.selected.replace(Some(device_key.clone()));
        let cancel = Arc::new(AtomicBool::new(false));
        *self.transfer.borrow_mut() = Some(TransferState {
            kind: TransferKind::Sending,
            cancel: cancel.clone(),
            session_id: None,
            total: 0,
            done: 0,
            receive_label: None,
        });

        self.transfer_title
            .set_label(&format!("Sending to {}", info.alias));
        self.transfer_file.set_label("Preparing files…");
        self.transfer_progress.set_fraction(0.0);
        self.transfer_detail.set_label("Connecting to the device…");
        self.transfer_cancel.set_sensitive(true);
        self.transfer_cancel.set_label("Cancel");
        self.content_stack.set_visible_child_name("transfer");
        self.window_title.set_subtitle("Sending Files");
        self.refresh_transfer_indicator();

        sender::spawn(self.core.clone(), device_key, paths, pin, cancel);
    }

    fn cancel_transfer(self: &Rc<Self>) {
        let Some(state) = self.transfer.borrow_mut().take() else {
            // Nothing is running, but the window may still show a stale
            // transfer page, so the button always leaves that view.
            self.back_to_device();
            return;
        };
        state.cancel.store(true, Ordering::SeqCst);
        if let Some(session_id) = &state.session_id {
            if let Some(session) = self.core.take_session(session_id) {
                session.canceled.store(true, Ordering::SeqCst);
                let core = self.core.clone();
                let session = session.clone();
                let _ = std::thread::Builder::new()
                    .name("localsend-recv-cancel".to_string())
                    .spawn(move || {
                        let Ok(agent) = crate::net::client(
                            &session.peer_protocol,
                            &session.peer_fingerprint,
                            core.identity().as_deref(),
                            None,
                        ) else {
                            return;
                        };
                        let url = format!(
                            "{}?sessionId={}",
                            crate::protocol::api_url(
                                &session.peer_ip,
                                session.peer_port,
                                &session.peer_protocol,
                                "/cancel",
                            ),
                            crate::protocol::percent_encode(&session.id),
                        );
                        let _ = agent
                            .post(&url)
                            .timeout(std::time::Duration::from_secs(3))
                            .call();
                    });
            }
        }
        self.queued_texts.borrow_mut().clear();
        self.queue_device.borrow_mut().take();
        self.refresh_transfer_indicator();
        self.toast("Transfer canceled");
        self.back_to_device();
    }

    /// Shows a message that arrived while the window was hidden, as long as
    /// the notification it belongs to is still recent.
    fn flush_pending_text(self: &Rc<Self>) {
        let Some(pending) = self.pending_text.borrow_mut().take() else {
            return;
        };
        if pending.received.elapsed() > Duration::from_secs(3600) {
            return;
        }
        self.window.present();
        self.show_text_messages(&pending.alias, pending.messages, true);
    }

    fn resolve_incoming_transfer(self: &Rc<Self>, accept: bool, session_id: &str) {
        let current = self
            .pending_decision
            .borrow()
            .as_ref()
            .map(|pending| pending.session_id.clone());
        if current.as_deref() != Some(session_id) {
            return;
        }
        self.answer_decision(accept);
    }

    fn is_receiving(&self, session_id: &str) -> bool {
        self.transfer
            .borrow()
            .as_ref()
            .map(|state| {
                matches!(state.kind, TransferKind::Receiving)
                    && state.session_id.as_deref() == Some(session_id)
            })
            .unwrap_or(false)
    }

    fn present_incoming(self: &Rc<Self>, request: IncomingRequest) {
        let files_label = receive_files_label(&request.files, request.messages.len());
        if self.core.config().quick_save {
            // Quick Save accepts immediately; the window is never stolen.
            let count = request.files.len().max(request.messages.len());
            let message = if count == 1 {
                format!("Receiving an item from {}", request.peer_alias)
            } else {
                format!("Receiving {count} items from {}", request.peer_alias)
            };
            self.begin_receiving(
                request.session_id.clone(),
                request.peer_alias.clone(),
                request.total_bytes,
                files_label,
            );
            self.notification_percent.set(u64::MAX);
            self.toast(&message);
            self.notify_transfer("Receiving", &message, false);
            return;
        }

        let count = request.files.len();
        let body = format!(
            "{count} file{} ({})",
            plural(count),
            util::human_bytes(request.total_bytes)
        );
        let session_id = request.session_id.clone();
        let alias = request.peer_alias.clone();
        self.pending_decision.borrow_mut().replace(PendingDecision {
            session_id: request.session_id,
            alias: request.peer_alias,
            files: count,
            total_bytes: request.total_bytes,
            files_label,
            sender: request.decision,
        });

        // A visible window always shows the question itself.
        if self.window.is_visible() {
            self.show_decision();
            return;
        }

        if let Some(app) = self.window.application() {
            let notification = gio::Notification::new(&format!("Incoming from {alias}"));
            notification.set_body(Some(&body));
            notification.add_button_with_target_value(
                "Decline",
                "app.decline-transfer",
                Some(&session_id.to_variant()),
            );
            notification.add_button_with_target_value(
                "Accept",
                "app.accept-transfer",
                Some(&session_id.to_variant()),
            );
            app.send_notification(Some(NOTIFICATION_ID), &notification);
        }
    }

    fn show_decision(self: &Rc<Self>) {
        let Some((alias, files, total)) = self
            .pending_decision
            .borrow()
            .as_ref()
            .map(|p| (p.alias.clone(), p.files, p.total_bytes))
        else {
            return;
        };
        self.decision_title
            .set_label(&format!("Incoming from {alias}"));
        self.decision_body.set_label(&format!(
            "{} file{} · {}\nSaved to {}",
            files,
            plural(files),
            util::human_bytes(total),
            self.core.config().download_dir_display()
        ));
        self.content_stack.set_visible_child_name("decision");
        self.window_title.set_subtitle("Incoming Transfer");
    }

    fn answer_decision(self: &Rc<Self>, accept: bool) {
        let Some(pending) = self.pending_decision.borrow_mut().take() else {
            return;
        };
        if let Some(app) = self.window.application() {
            app.withdraw_notification(NOTIFICATION_ID);
        }
        if accept {
            self.begin_receiving(
                pending.session_id.clone(),
                pending.alias.clone(),
                pending.total_bytes,
                pending.files_label,
            );
            self.toast(&format!("Receiving from {}", pending.alias));
        } else {
            self.toast(&format!("Declined the transfer from {}", pending.alias));
            self.back_to_device();
        }
        let _ = pending.sender.send(accept);
    }

    fn show_text_messages(
        self: &Rc<Self>,
        alias: &str,
        messages: Vec<String>,
        return_to_device: bool,
    ) -> bool {
        if messages.is_empty() {
            return false;
        }
        let joined = messages.join("\n\n");
        let view = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .left_margin(12)
            .right_margin(12)
            .top_margin(10)
            .bottom_margin(10)
            .build();
        view.buffer().set_text(&joined);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .overlay_scrolling(false)
            .min_content_height(160)
            .max_content_height(360)
            .propagate_natural_height(true)
            .child(&view)
            .build();
        scroller.add_css_class("text-area");

        let heading = if messages.len() == 1 {
            "Message"
        } else {
            "Messages"
        };
        let dialog = adw::AlertDialog::builder()
            .heading(heading)
            .body(format!("From {alias}"))
            .extra_child(&scroller)
            .default_response("copy")
            .close_response("close")
            .build();
        dialog.add_response("close", "Done");
        dialog.add_response("copy", "Copy");
        dialog.set_response_appearance("copy", adw::ResponseAppearance::Suggested);

        let ui = self.clone();
        let copy_text = joined.clone();
        dialog.choose(
            Some(&self.window),
            gio::Cancellable::NONE,
            move |response| {
                if response.as_str() == "copy" {
                    ui.copy_text(&copy_text);
                    ui.toast("Message copied");
                }
                if return_to_device {
                    ui.back_to_device();
                }
            },
        );
        true
    }

    fn begin_receiving(
        self: &Rc<Self>,
        session_id: String,
        alias: String,
        total: u64,
        files_label: String,
    ) {
        if self.transfer.borrow().is_some() {
            return;
        }
        *self.transfer.borrow_mut() = Some(TransferState {
            kind: TransferKind::Receiving,
            cancel: Arc::new(AtomicBool::new(false)),
            session_id: Some(session_id),
            total,
            done: 0,
            receive_label: Some(files_label.clone()),
        });
        self.transfer_title
            .set_label(&format!("Receiving from {alias}"));
        self.transfer_file.set_label(&files_label);
        self.transfer_progress.set_fraction(0.0);
        self.transfer_detail
            .set_label(&format!("0 B of {}", util::human_bytes(total)));
        self.transfer_cancel.set_sensitive(true);
        self.transfer_cancel.set_label("Cancel");
        self.content_stack.set_visible_child_name("transfer");
        self.window_title.set_subtitle("Receiving Files");
        self.refresh_transfer_indicator();
    }

    fn show_pin_dialog(self: &Rc<Self>, device: String, paths: Vec<PathBuf>, text: Option<String>) {
        let entry = gtk::PasswordEntry::new();
        entry.set_placeholder_text(Some("PIN"));
        entry.set_width_request(240);
        let is_text = text.is_some();
        let dialog = adw::AlertDialog::builder()
            .heading("PIN Required")
            .body(if is_text {
                "Enter the PIN shown on the other device to send your message."
            } else {
                "Enter the PIN shown on the other device to send your files."
            })
            .extra_child(&entry)
            .default_response("send")
            .close_response("cancel")
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("send", "Send");
        dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);

        let ui = self.clone();
        dialog.choose(
            Some(&self.window),
            gio::Cancellable::NONE,
            move |response| {
                if response.as_str() == "send" {
                    let pin = entry.text().to_string();
                    if pin.is_empty() {
                        ui.toast("The PIN cannot be empty");
                    } else if let Some(text) = text {
                        ui.start_send_text(device, text, Some(pin));
                    } else {
                        ui.start_send(device, paths, Some(pin));
                    }
                }
            },
        );
    }

    fn show_history(self: &Rc<Self>) {
        if let Some((dialog, page)) = self.history_dialog.borrow().clone() {
            self.rebuild_history_page(&page, &dialog);
            dialog.present(Some(&self.window));
            return;
        }

        let dialog = adw::PreferencesDialog::new();
        dialog.set_title("History");
        dialog.set_content_width(600);
        dialog.set_content_height(700);
        let page = adw::PreferencesPage::new();
        dialog.add(&page);

        dialog.connect_closed({
            let ui = self.clone();
            move |_| {
                ui.history_dialog.borrow_mut().take();
                ui.history_groups.borrow_mut().clear();
                ui.history_expanded.borrow_mut().clear();
                Ui::trim_memory();
                glib::idle_add_local_once(Ui::trim_memory);
            }
        });

        *self.history_dialog.borrow_mut() = Some((dialog.clone(), page.clone()));
        self.rebuild_history_page(&page, &dialog);
        dialog.present(Some(&self.window));
    }

    fn refresh_history_dialog(self: &Rc<Self>) {
        // Nothing to update when the view is closed: opening it always rebuilds
        // from the current snapshot, so the content can never be stale. Toasting
        // here fired once per received file and buried the transfer UI.
        let Some(_open) = self.history_dialog.borrow().clone() else {
            return;
        };
        // Coalesce bursts: a 198-file transfer emits 198 HistoryChanged events,
        // and rebuilding the whole page for each one stalls the main loop.
        if self.history_refresh_queued.replace(true) {
            return;
        }
        let ui = self.clone();
        glib::idle_add_local_once(move || {
            ui.history_refresh_queued.set(false);
            let Some((dialog, page)) = ui.history_dialog.borrow().clone() else {
                return;
            };
            ui.rebuild_history_page(&page, &dialog);
        });
    }

    fn rebuild_history_page(
        self: &Rc<Self>,
        page: &adw::PreferencesPage,
        dialog: &adw::PreferencesDialog,
    ) {
        dialog.set_title("History");

        let entries = self.core.history_snapshot();
        let live_ids: HashSet<String> = entries.iter().map(|e| e.id.clone()).collect();
        self.history_expanded
            .borrow_mut()
            .retain(|id| live_ids.contains(id));

        for group in self.history_groups.borrow_mut().drain(..) {
            page.remove(&group);
        }

        if entries.is_empty() {
            let group = adw::PreferencesGroup::new();
            let status = adw::StatusPage::builder()
                .icon_name("document-open-recent-symbolic")
                .title("No History Yet")
                .description("Files and messages you receive will appear here.")
                .build();
            status.set_vexpand(true);
            group.add(&status);
            page.add(&group);
            self.history_groups.borrow_mut().push(group);
            return;
        }

        let mut current_day: Option<String> = None;
        let mut group: Option<adw::PreferencesGroup> = None;
        for entry in &entries {
            let day = day_label(entry.timestamp);
            if current_day.as_deref() != Some(day.as_str()) {
                let g = adw::PreferencesGroup::builder().title(&day).build();
                page.add(&g);
                self.history_groups.borrow_mut().push(g.clone());
                group = Some(g);
                current_day = Some(day);
            }
            let Some(g) = group.as_ref() else {
                continue;
            };

            let is_expanded = self.history_expanded.borrow().contains(&entry.id);
            let row = self.build_history_entry_row(entry, dialog, is_expanded);
            g.add(&row);
        }
    }

    fn build_history_entry_row(
        self: &Rc<Self>,
        entry: &HistoryEntry,
        dialog: &adw::PreferencesDialog,
        restore_expanded: bool,
    ) -> adw::ExpanderRow {
        let title = transfer_title(entry);
        let subtitle = format!(
            "From {} · {} · {}",
            entry.alias,
            time_label(entry.timestamp),
            util::human_bytes(entry_total_bytes(entry))
        );
        let row = adw::ExpanderRow::builder()
            .title(&title)
            .subtitle(&subtitle)
            .build();
        // A transfer can contain hundreds of files. Building rows for every
        // entry upfront made opening/scrolling laggy, so populate lazily and in
        // small chunks to keep expand/collapse responsive.
        let items = Rc::new(entry.items.clone());
        let alias = Rc::new(entry.alias.clone());
        let dialog = dialog.clone();
        let ui = self.clone();
        let entry_id = entry.id.clone();
        let populated = Rc::new(Cell::new(false));

        row.connect_notify_local(Some("expanded"), {
            let ui = ui.clone();
            let entry_id = entry_id.clone();
            let items = items.clone();
            let alias = alias.clone();
            let dialog = dialog.clone();
            let populated = populated.clone();
            move |row, _| {
                if !row.is_expanded() {
                    ui.history_expanded.borrow_mut().remove(&entry_id);
                    return;
                }
                ui.history_expanded.borrow_mut().insert(entry_id.clone());
                if populated.replace(true) {
                    return;
                }
                // Filling in one pass lets GTK queue a single layout instead of
                // one per idle tick, which made expanding large transfers janky.
                for item in items.iter() {
                    let child = ui.build_history_item_row(&alias, item, &dialog);
                    row.add_row(&child);
                }
            }
        });

        if restore_expanded {
            row.set_expanded(true);
        }

        row
    }

    fn build_history_item_row(
        self: &Rc<Self>,
        alias: &str,
        item: &HistoryItem,
        dialog: &adw::PreferencesDialog,
    ) -> adw::ActionRow {
        let row = adw::ActionRow::new();
        // Checking whether a file still exists hits the filesystem, so resolve
        // once per row instead of once per use.
        let resolution = item.resolution();
        match item {
            HistoryItem::File { name, .. } => {
                let icon = gtk::Image::from_icon_name("text-x-generic-symbolic");
                row.add_prefix(&icon);
                row.set_title(name);
                row.set_subtitle(&item_subtitle(item, &resolution));
                if matches!(resolution, Resolution::Missing) {
                    row.add_css_class("dim-label");
                } else {
                    row.set_activatable(true);
                    let ui_open = self.clone();
                    let item_id_open = item.id().to_string();
                    let dlg_open = dialog.clone();
                    row.connect_activated(move |_| {
                        ui_open.history_open_item(&item_id_open, &dlg_open);
                    });
                }

                row.add_suffix(&history_item_menu("Copy Path", item.id()));
            }
            HistoryItem::Message { text, .. } => {
                let icon = gtk::Image::from_icon_name("mail-message-new-symbolic");
                row.add_prefix(&icon);
                let preview = text
                    .as_deref()
                    .map(first_line)
                    .unwrap_or_else(|| "Message removed from cache".to_string());
                row.set_title(&preview);
                row.set_subtitle(if text.is_some() {
                    "Text message"
                } else {
                    "No longer available"
                });

                if text.is_some() {
                    row.set_activatable(true);
                    let ui_show = self.clone();
                    let item_id_show = item.id().to_string();
                    let alias_s = alias.to_string();
                    row.connect_activated(move |_| {
                        ui_show.history_show_message(&item_id_show, &alias_s);
                    });
                }

                row.add_suffix(&history_item_menu("Copy Text", item.id()));
            }
        }

        let del_btn = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text("Delete…")
            .build();
        del_btn.add_css_class("flat");
        let item_id = item.id().to_string();
        let is_file = matches!(item, HistoryItem::File { .. });
        let ui = self.clone();
        del_btn.connect_clicked(move |_| {
            ui.history_delete_dialog(&item_id, is_file);
        });
        row.add_suffix(&del_btn);
        row
    }

    fn history_copy_item(self: &Rc<Self>, item_id: &str) {
        match self.core.history_resolve(item_id) {
            Resolution::Original(path) | Resolution::Cached(path) => {
                self.copy_text(&path.to_string_lossy());
                self.toast("Path copied");
            }
            Resolution::Message => self.history_copy_message(item_id),
            Resolution::Missing => {
                let Some((dialog, _)) = self.history_dialog.borrow().clone() else {
                    self.toast("No longer available");
                    return;
                };
                let alert = adw::AlertDialog::builder()
                    .heading("No longer available")
                    .body("This file is gone from your downloads and there is no cached copy left.")
                    .build();
                alert.add_response("ok", "OK");
                alert.set_default_response(Some("ok"));
                alert.present(Some(&dialog));
            }
        }
    }

    fn history_open_item(self: &Rc<Self>, item_id: &str, dialog: &adw::PreferencesDialog) {
        match self.core.history_resolve(item_id) {
            Resolution::Original(path) => {
                let uri = format!("file://{}", path.to_string_lossy());
                if gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>)
                    .is_err()
                {
                    self.toast("Could not open this file");
                }
            }
            Resolution::Cached(path) => {
                let uri = format!("file://{}", path.to_string_lossy());
                if gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>)
                    .is_err()
                {
                    self.toast("Could not open this file");
                } else {
                    self.toast("Opening the cached copy");
                }
            }
            Resolution::Message => self.toast("Messages are not files"),
            Resolution::Missing => {
                let alert = adw::AlertDialog::builder()
                    .heading("No longer available")
                    .body("This file is gone from your downloads and there is no cached copy left.")
                    .build();
                alert.add_response("ok", "OK");
                alert.set_default_response(Some("ok"));
                alert.present(Some(dialog));
            }
        }
    }

    fn history_copy_message(&self, item_id: &str) {
        if let Some(HistoryItem::Message {
            text: Some(text), ..
        }) = self
            .core
            .history_snapshot()
            .iter()
            .flat_map(|e| e.items.iter())
            .find(|i| i.id() == item_id)
        {
            self.copy_text(text);
            self.toast("Message copied");
        } else {
            self.toast("Message is no longer available");
        }
    }

    fn history_show_message(self: &Rc<Self>, item_id: &str, alias: &str) {
        if let Some(HistoryItem::Message {
            text: Some(text), ..
        }) = self
            .core
            .history_snapshot()
            .iter()
            .flat_map(|e| e.items.iter())
            .find(|i| i.id() == item_id)
            .cloned()
        {
            self.show_text_messages(alias, vec![text], false);
        } else {
            self.toast("Message is no longer available");
        }
    }

    fn history_delete_dialog(self: &Rc<Self>, item_id: &str, is_file: bool) {
        let alert = adw::AlertDialog::builder()
            .heading("Remove from History")
            .body(if is_file {
                "Remove this item from history and its cached copy. You can also delete the original file from disk."
            } else {
                "Remove this message from history?"
            })
            .build();
        alert.add_response("cancel", "Cancel");
        alert.add_response("remove", "Remove from History");
        if is_file {
            alert.add_response("delete_file", "Delete File");
            alert.set_response_appearance("delete_file", adw::ResponseAppearance::Destructive);
        }
        alert.set_default_response(Some("cancel"));
        alert.set_close_response("cancel");
        let ui = self.clone();
        let id = item_id.to_string();
        // Present on the main window so the confirmation stays above the
        // history dialog. Window-level toasts are hidden behind that dialog,
        // so success is shown by refreshing the list and errors get an alert.
        alert.choose(
            Some(&self.window),
            gio::Cancellable::NONE,
            move |resp| match resp.as_str() {
                "remove" => {
                    ui.core.history_remove_item(&id);
                    let ui_refresh = ui.clone();
                    gtk::glib::idle_add_local_once(move || {
                        ui_refresh.refresh_history_dialog();
                    });
                }
                "delete_file" => {
                    let result = ui.core.history_delete_file(&id);
                    let ui_refresh = ui.clone();
                    gtk::glib::idle_add_local_once(move || {
                        ui_refresh.refresh_history_dialog();
                        if let Err(err) = result {
                            let error = adw::AlertDialog::builder()
                                .heading("Could Not Delete File")
                                .body(&err)
                                .build();
                            error.add_response("ok", "OK");
                            error.set_default_response(Some("ok"));
                            error.present(Some(&ui_refresh.window));
                        }
                    });
                }
                _ => {}
            },
        );
    }

    fn show_settings(self: &Rc<Self>) {
        let config = self.core.config();
        let dialog = adw::PreferencesDialog::new();
        dialog.set_title("Settings");

        let page = adw::PreferencesPage::new();

        let device_group = adw::PreferencesGroup::builder()
            .title("Device")
            .description("Network changes take effect after restarting LocalSend.")
            .build();

        let name_row = adw::EntryRow::builder().title("Device Name").build();
        name_row.set_text(&config.alias);
        name_row.connect_changed({
            let core = self.core.clone();
            move |row| {
                let text = row.text().trim().to_string();
                if !text.is_empty() {
                    core.update_config(|config| config.alias = text);
                }
            }
        });
        name_row.connect_apply({
            let core = self.core.clone();
            move |row| {
                let text = row.text().trim().to_string();
                if text.is_empty() {
                    row.set_text(&core.config().alias);
                } else {
                    core.update_config(|config| config.alias = text);
                }
            }
        });
        device_group.add(&name_row);

        let type_names =
            gtk::StringList::new(&["Desktop", "Phone", "Web Browser", "Headless", "Server"]);
        let type_row = adw::ComboRow::builder()
            .title("Device Type")
            .model(&type_names)
            .build();
        let type_index: u32 = match config.device_type.as_str() {
            "mobile" => 1,
            "web" => 2,
            "headless" => 3,
            "server" => 4,
            _ => 0,
        };
        type_row.set_selected(type_index);
        type_row.connect_notify_local(Some("selected"), {
            let core = self.core.clone();
            move |row, _| {
                let value = match row.selected() {
                    1 => "mobile",
                    2 => "web",
                    3 => "headless",
                    4 => "server",
                    _ => "desktop",
                };
                core.update_config(|config| config.device_type = value.to_string());
            }
        });
        device_group.add(&type_row);

        let port_row = adw::EntryRow::builder().title("Receiving Port").build();
        port_row.set_text(&config.port.to_string());
        port_row.connect_changed({
            let core = self.core.clone();
            move |row| {
                if let Ok(port) = row.text().to_string().parse::<u16>() {
                    if port >= 1024 && core.config().port != port {
                        core.update_config(|config| config.port = port);
                    }
                }
            }
        });
        device_group.add(&port_row);
        page.add(&device_group);

        let receiving_group = adw::PreferencesGroup::builder()
            .title("Receiving")
            .description("Ask the sender for these settings.")
            .build();

        let pin_row = adw::PasswordEntryRow::builder()
            .title("PIN (optional)")
            .build();
        pin_row.set_text(&config.pin);
        pin_row.connect_changed({
            let core = self.core.clone();
            move |row| {
                let pin: String = row
                    .text()
                    .chars()
                    .filter(|character| character.is_ascii_digit())
                    .take(12)
                    .collect();
                core.update_config(|config| config.pin = pin);
            }
        });
        receiving_group.add(&pin_row);

        let folder_row = adw::ActionRow::builder()
            .title("Save Location")
            .subtitle(config.download_dir_display())
            .build();
        let choose_button = gtk::Button::with_label("Choose…");
        choose_button.add_css_class("flat");
        choose_button.set_valign(gtk::Align::Center);
        folder_row.add_suffix(&choose_button);
        folder_row.set_activatable_widget(Some(&choose_button));
        choose_button.connect_clicked({
            let core = self.core.clone();
            let window = self.window.clone();
            let folder_row = folder_row.clone();
            move |_| {
                let dialog = gtk::FileDialog::builder()
                    .title("Choose Download Folder")
                    .modal(true)
                    .build();
                let core = core.clone();
                let folder_row = folder_row.clone();
                let window = window.clone();
                glib::spawn_future_local(async move {
                    if let Ok(folder) = dialog.select_folder_future(Some(&window)).await {
                        if let Some(path) = folder.path() {
                            core.update_config(|config| config.download_dir = path.clone());
                            folder_row.set_subtitle(&crate::config::display_path(&path));
                        }
                    }
                });
            }
        });
        receiving_group.add(&folder_row);

        let encryption_row = adw::SwitchRow::builder()
            .title("Encrypt Transfers")
            .subtitle("Uses a self-signed certificate. Change takes effect after restarting.")
            .build();
        encryption_row.set_active(config.encryption);
        encryption_row.connect_active_notify({
            let core = self.core.clone();
            move |row| {
                core.update_config(|config| config.encryption = row.is_active());
            }
        });
        receiving_group.add(&encryption_row);

        page.add(&receiving_group);

        let history_group = adw::PreferencesGroup::builder().title("History").build();

        let history_cache_row = adw::SwitchRow::builder()
            .title("Cache Received Files in History")
            .subtitle("Keep cached copies of received files (up to 128 MB) so they stay available from History.")
            .build();
        history_cache_row.set_active(config.history_cache);
        history_cache_row.connect_active_notify({
            let core = self.core.clone();
            move |row| {
                core.update_config(|config| config.history_cache = row.is_active());
            }
        });
        history_group.add(&history_cache_row);

        let usage = util::human_bytes(self.core.history_usage());
        let history_size_row = adw::ActionRow::builder()
            .title("History Cache")
            .subtitle(&format!("{usage} used"))
            .build();
        let clear_history_button = gtk::Button::builder()
            .label("Clear")
            .valign(gtk::Align::Center)
            .build();
        clear_history_button.add_css_class("destructive-action");
        {
            let core = self.core.clone();
            let history_size_row = history_size_row.clone();
            clear_history_button.connect_clicked(move |_| {
                core.history_clear_all();
                let usage = util::human_bytes(core.history_usage());
                history_size_row.set_subtitle(&format!("{usage} used"));
            });
        }
        history_size_row.add_suffix(&clear_history_button);
        history_size_row.set_activatable_widget(Some(&clear_history_button));
        history_group.add(&history_size_row);
        page.add(&history_group);

        let background_group =
            adw::PreferencesGroup::builder()
                .title("Background &amp; Tray")
                .description(
                    "The tray icon is only shown when a StatusNotifierWatcher is available on the session bus.",
                )
                .build();

        let background_row = adw::SwitchRow::builder()
            .title("Run in Background")
            .subtitle("Keep sharing and discovery alive after the window is closed.")
            .build();
        background_row.set_active(config.run_in_background);
        background_row.connect_active_notify({
            let core = self.core.clone();
            let ui = self.clone();
            move |row| {
                let enabled = row.is_active();
                core.update_config(|config| config.run_in_background = enabled);
                ui.set_background(enabled);
            }
        });
        background_group.add(&background_row);

        let tray_row = adw::SwitchRow::builder()
            .title("Show Tray Icon")
            .subtitle("Needs a StatusNotifierWatcher on the desktop; takes effect after a restart.")
            .build();
        tray_row.set_active(config.tray_icon);
        tray_row.connect_active_notify({
            let core = self.core.clone();
            let ui = self.clone();
            move |row| {
                core.update_config(|config| config.tray_icon = row.is_active());
                ui.set_background(ui.core.config().run_in_background);
            }
        });
        background_group.add(&tray_row);

        let start_row = adw::SwitchRow::builder()
            .title("Start in Background")
            .subtitle("Launch without showing the window when the tray icon is available.")
            .build();
        start_row.set_active(config.start_in_background);
        start_row.connect_active_notify({
            let core = self.core.clone();
            move |row| {
                core.update_config(|config| config.start_in_background = row.is_active());
            }
        });
        background_group.add(&start_row);

        let quick_row = adw::SwitchRow::builder()
            .title("Quick Save")
            .subtitle(
                "Receiving never asks for confirmation, so transfers are always accepted. Quick Save keeps the window closed and reports progress through notifications.",
            )
            .build();
        quick_row.set_active(config.quick_save);
        quick_row.connect_active_notify({
            let core = self.core.clone();
            move |row| {
                core.update_config(|config| config.quick_save = row.is_active());
            }
        });
        background_group.add(&quick_row);

        page.add(&background_group);
        dialog.add(&page);
        dialog.connect_closed(|_| {
            Ui::trim_memory();
            glib::idle_add_local_once(Ui::trim_memory);
        });
        dialog.present(Some(&self.window));
    }

    fn show_shortcuts(&self) {
        let builder = gtk::Builder::from_string(SHORTCUTS_XML);
        let shortcuts: Option<gtk::ShortcutsWindow> = builder.object("shortcuts");
        if let Some(shortcuts) = shortcuts {
            shortcuts.set_transient_for(Some(&self.window));
            shortcuts.present();
        }
    }

    fn show_about(&self) {
        let dialog = adw::AboutDialog::builder()
            .application_name("LocalSend")
            .application_icon(APP_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .developer_name("LocalSend Adwaita Contributors")
            .comments("Share files with nearby devices, without a server or an account.")
            .website("https://github.com/bazixv13/localsend-adwaita")
            .support_url("https://github.com/bazixv13/localsend-adwaita")
            .issue_url("https://github.com/bazixv13/localsend-adwaita/issues")
            .license_type(gtk::License::Gpl30)
            .build();
        dialog.add_credit_section(
            Some("Based on LocalSend by"),
            &["The LocalSend contributors"],
        );
        dialog.connect_closed(|_| {
            Ui::trim_memory();
            glib::idle_add_local_once(Ui::trim_memory);
        });
        dialog.present(Some(&self.window));
    }

    fn toast(&self, message: &str) {
        let toast = adw::Toast::new(message);
        toast.set_timeout(4);
        self.toasts.add_toast(toast);
    }

    fn copy_text(&self, text: &str) {
        if let Some(display) = gdk::Display::default() {
            display.clipboard().set_text(text);
        }
    }
}

fn progress_percent(done: u64, total: u64) -> u64 {
    if total == 0 {
        0
    } else {
        ((done.saturating_mul(100)) / total).min(100)
    }
}

/// Shortens a message so it fits in a notification body.
fn summarize(text: &str) -> String {
    let text = text.trim();
    let mut out: String = text.chars().take(180).collect();
    if out.chars().count() < text.chars().count() {
        out.push('…');
    }
    out
}

fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

fn open_folder(path: &Path) {
    let uri = gio::File::for_path(path).uri();
    if let Err(error) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
        eprintln!("Could not open {uri}: {error}");
    }
}

const SHORTCUTS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<interface>
  <object class="GtkShortcutsWindow" id="shortcuts">
    <property name="modal">True</property>
    <child>
      <object class="GtkShortcutsSection">
        <property name="section-name">general</property>
        <property name="max-height">12</property>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Transfers</property>
            <child>
              <object class="GtkShortcutsShortcut">
                <property name="title">Send Files</property>
                <property name="accelerator">&lt;Control&gt;o</property>
              </object>
            </child>
            <child>
              <object class="GtkShortcutsShortcut">
                <property name="title">Send a Folder</property>
                <property name="accelerator">&lt;Control&gt;&lt;Shift&gt;o</property>
              </object>
            </child>
            <child>
              <object class="GtkShortcutsShortcut">
                <property name="title">Send Text</property>
                <property name="accelerator">&lt;Control&gt;&lt;Shift&gt;t</property>
              </object>
            </child>
          </object>
        </child>
        <child>
          <object class="GtkShortcutsGroup">
            <property name="title">Application</property>
            <child>
              <object class="GtkShortcutsShortcut">
                <property name="title">Settings</property>
                <property name="accelerator">&lt;Control&gt;comma</property>
              </object>
            </child>
            <child>
              <object class="GtkShortcutsShortcut">
                <property name="title">Keyboard Shortcuts</property>
                <property name="accelerator">F1</property>
              </object>
            </child>
            <child>
              <object class="GtkShortcutsShortcut">
                <property name="title">Quit</property>
                <property name="accelerator">&lt;Control&gt;q</property>
              </object>
            </child>
          </object>
        </child>
      </object>
    </child>
  </object>
</interface>
"#;

fn day_label(ts: i64) -> String {
    let (Ok(now), Ok(dt)) = (
        glib::DateTime::now_local(),
        glib::DateTime::from_unix_local(ts),
    ) else {
        return "Earlier".to_string();
    };
    let that = (dt.year(), dt.month(), dt.day_of_month());
    if that == (now.year(), now.month(), now.day_of_month()) {
        return "Today".to_string();
    }
    if let Ok(yesterday) = now.add_days(-1) {
        if that
            == (
                yesterday.year(),
                yesterday.month(),
                yesterday.day_of_month(),
            )
        {
            return "Yesterday".to_string();
        }
    }
    dt.format("%e %B %Y")
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "Earlier".to_string())
}

fn time_label(ts: i64) -> String {
    glib::DateTime::from_unix_local(ts)
        .ok()
        .and_then(|dt| dt.format("%H:%M").ok().map(|s| s.to_string()))
        .unwrap_or_default()
}

fn transfer_title(entry: &HistoryEntry) -> String {
    let mut files = 0usize;
    let mut messages = 0usize;
    for item in &entry.items {
        match item {
            HistoryItem::File { .. } => files += 1,
            HistoryItem::Message { .. } => messages += 1,
        }
    }
    match (files, messages) {
        (0, 1) => "Message".to_string(),
        (0, m) => format!("{m} messages"),
        (1, 0) => "1 file".to_string(),
        (f, 0) => format!("{f} files"),
        (f, m) => format!(
            "{f} file{} and {m} message{}",
            if f == 1 { "" } else { "s" },
            if m == 1 { "" } else { "s" }
        ),
    }
}

fn receive_files_label(files: &[IncomingFileInfo], message_count: usize) -> String {
    let count = files.len();
    if count == 0 {
        return match message_count {
            0 => "Receiving…".to_string(),
            1 => "Message".to_string(),
            n => format!("{n} messages"),
        };
    }
    if count == 1 {
        let name = &files[0].name;
        if name.chars().count() <= 48 {
            return name.clone();
        }
        return "1 file".to_string();
    }
    if count > 8 {
        return format!("{count} files");
    }
    let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
    let too_long = names.iter().any(|n| n.chars().count() > 28);
    let joined = names.join(", ");
    if too_long || joined.chars().count() > 60 {
        format!("{count} files")
    } else {
        joined
    }
}

fn queue_icon_for_paths(paths: &[PathBuf]) -> &'static str {
    match paths {
        [] => "document-symbolic",
        [path] if path.is_dir() => "folder-symbolic",
        [path] => icon_for_path(path),
        _ if paths.iter().all(|path| path.is_dir()) => "folder-symbolic",
        _ => "emblem-documents-symbolic",
    }
}

fn icon_for_path(path: &Path) -> &'static str {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    match (mime.type_().as_str(), mime.subtype().as_str()) {
        ("image", _) => "image-x-generic-symbolic",
        ("audio", _) => "audio-x-generic-symbolic",
        ("video", _) => "video-x-generic-symbolic",
        ("text", _) => "text-x-generic-symbolic",
        ("application", "pdf") => "x-office-document-symbolic",
        (
            "application",
            "zip" | "x-tar" | "gzip" | "x-gzip" | "x-7z-compressed" | "x-rar-compressed" | "x-xz"
            | "x-bzip" | "x-bzip2" | "zstd",
        ) => "package-x-generic-symbolic",
        (
            "application",
            "vnd.oasis.opendocument.text"
            | "vnd.oasis.opendocument.spreadsheet"
            | "vnd.oasis.opendocument.presentation"
            | "msword"
            | "vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "vnd.ms-excel"
            | "vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "vnd.ms-powerpoint"
            | "vnd.openxmlformats-officedocument.presentationml.presentation"
            | "rtf",
        ) => "x-office-document-symbolic",
        _ => "document-symbolic",
    }
}

fn entry_total_bytes(entry: &HistoryEntry) -> u64 {
    entry
        .items
        .iter()
        .map(|i| match i {
            HistoryItem::File { size, .. } => *size,
            HistoryItem::Message { bytes, .. } => *bytes,
        })
        .sum()
}

/// A menu button backed by a menu model, so the popup itself is only built when
/// the menu is opened. A popover with a real button per row was the main cost
/// of expanding a transfer with hundreds of files.
fn history_item_menu(label: &str, item_id: &str) -> gtk::MenuButton {
    let menu = gio::Menu::new();
    let entry = gio::MenuItem::new(Some(label), None);
    entry.set_action_and_target_value(Some("win.history-copy-item"), Some(&item_id.to_variant()));
    menu.append_item(&entry);
    let button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .valign(gtk::Align::Center)
        .tooltip_text("More options")
        .menu_model(&menu)
        .build();
    button.add_css_class("flat");
    button
}

fn item_subtitle(item: &HistoryItem, resolution: &Resolution) -> String {
    match item {
        HistoryItem::File { size, path, .. } => {
            let size = util::human_bytes(*size);
            match resolution {
                Resolution::Original(_) => {
                    format!("{size} · {}", crate::config::display_path(path))
                }
                Resolution::Cached(_) => format!("{size} · Cached copy"),
                Resolution::Message | Resolution::Missing => {
                    format!("{size} · No longer available")
                }
            }
        }
        HistoryItem::Message { text, .. } => {
            if text.is_some() {
                "Text message".to_string()
            } else {
                "No longer available".to_string()
            }
        }
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    let mut out = line.to_string();
    if out.chars().count() > 60 {
        out = out.chars().take(57).collect::<String>() + "…";
    }
    if out.is_empty() {
        "Message".to_string()
    } else {
        out
    }
}
