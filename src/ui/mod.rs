//! GTK4/libadwaita chat window: a transcript of sent and received overs, an input
//! row, RX level and speed, and TX/RX settings.

use crate::{EngineArgs, ToneArgs, render};
use adw::prelude::*;
use cw_chat::{
    audio::engine::{self, Command, Engine, Event},
    morse::encoder,
    rx::decoder::{RxEvent, Status},
};
use gtk::{gdk, gio, glib};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

const APP_ID: &str = "net.cwchat.CwChat";

const CSS: &str = "
.bubble { padding: 8px 12px; border-radius: 12px; }
.bubble.tx { background-color: alpha(@accent_bg_color, 0.18); }
.bubble.rx { background-color: alpha(@view_fg_color, 0.07); }
.bubble .message { font-size: 1.15em; }
.keyed { color: @success_color; }
.idle-key { color: alpha(@view_fg_color, 0.25); }
";

pub fn run(engine: EngineArgs, tone: ToneArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Each launch is its own instance, so several copies can talk to each other.
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|app| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        app.set_accels_for_action("window.close", &["<Ctrl>q", "<Ctrl>w"]);
    });
    app.connect_activate(move |app| build(app, engine.clone(), tone.clone()));
    // Our options were parsed by clap; GTK gets none.
    let status = app.run_with_args::<&str>(&[]);
    if status != glib::ExitCode::SUCCESS {
        return Err("GTK application exited with an error".into());
    }
    Ok(())
}

struct TxRow {
    status: gtk::Label,
    progress: gtk::ProgressBar,
}

struct RxRow {
    header: gtk::Label,
    text: gtk::Label,
    content: String,
}

struct State {
    engine: Option<Engine>,
    tone: ToneArgs,
    next_id: u64,
    tx_rows: HashMap<u64, TxRow>,
    rx_row: Option<RxRow>,
    rx_wpm: f64,
    pending: usize,
}

#[derive(Clone)]
struct Ui {
    transcript: gtk::Box,
    scroller: gtk::ScrolledWindow,
    empty: adw::StatusPage,
    entry: gtk::Entry,
    send: gtk::Button,
    stop: gtk::Button,
    level: gtk::LevelBar,
    key: gtk::Label,
    rx_info: gtk::Label,
    toasts: adw::ToastOverlay,
    banner: adw::Banner,
}

fn build(app: &adw::Application, engine_args: EngineArgs, tone: ToneArgs) {
    let config = engine_args.config(&tone);
    let title = match &config.name {
        Some(name) => format!("cw-chat — {name}"),
        None => "cw-chat".to_owned(),
    };
    let window_title = adw::WindowTitle::new(
        &title,
        &format!("{} · {}", config.node_name("tx"), config.node_name("rx")),
    );

    // Transcript.
    let transcript = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .valign(gtk::Align::End)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&transcript)
        .visible(false)
        .build();
    let empty = adw::StatusPage::builder()
        .icon_name("audio-input-microphone-symbolic")
        .title("No messages yet")
        .description(format!(
            "Type below to send CW. Received CW appears here as it is decoded.\nRX listens at {} Hz.",
            config.rx_tone
        ))
        .vexpand(true)
        .build();

    // RX meter row.
    let key = gtk::Label::builder()
        .label("●")
        .css_classes(["idle-key"])
        .tooltip_text("Lit while a tone is keyed")
        .build();
    let level = gtk::LevelBar::builder()
        .min_value(0.0)
        .max_value(1.0)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .tooltip_text("RX tone level (−80 to 0 dBFS)")
        .build();
    // The default offsets colour a full bar as a warning; a strong tone is good here.
    for offset in ["low", "high", "full"] {
        level.remove_offset_value(Some(offset));
    }
    let rx_info = gtk::Label::builder()
        .label(format!("RX {:.0} Hz", config.rx_tone))
        .css_classes(["caption", "dim-label", "numeric"])
        .width_chars(18)
        .xalign(1.0)
        .build();
    let stop = gtk::Button::builder()
        .label("Stop")
        .css_classes(["destructive-action"])
        .tooltip_text("Stop sending and clear the queue (Esc)")
        .visible(false)
        .build();
    let meter = gtk::Box::builder()
        .spacing(8)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .build();
    meter.append(
        &gtk::Label::builder()
            .label("RX")
            .css_classes(["caption-heading"])
            .build(),
    );
    meter.append(&key);
    meter.append(&level);
    meter.append(&rx_info);
    meter.append(&stop);

    // Input row.
    let entry = gtk::Entry::builder()
        .placeholder_text("Type a message and press Enter")
        .hexpand(true)
        .max_length(4096)
        .build();
    let send = gtk::Button::builder()
        .label("Send")
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();
    let input = gtk::Box::builder()
        .spacing(8)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(12)
        .build();
    input.append(&entry);
    input.append(&send);

    let banner = adw::Banner::builder().revealed(false).build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&banner);
    content.append(&empty);
    content.append(&scroller);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&meter);
    content.append(&input);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&content));

    let ui = Ui {
        transcript,
        scroller,
        empty,
        entry: entry.clone(),
        send: send.clone(),
        stop: stop.clone(),
        level,
        key,
        rx_info,
        toasts: toasts.clone(),
        banner,
    };

    // Engine, with events forwarded to this thread.
    let (sender, receiver) = async_channel::unbounded();
    let engine = match Engine::start(config.clone(), move |event| {
        let _ = sender.try_send(event);
    }) {
        Ok(engine) => Some(engine),
        Err(error) => {
            show_banner(&ui, &format!("Audio unavailable: {error}"));
            None
        }
    };
    let state = Rc::new(RefCell::new(State {
        engine,
        tone: tone.clone(),
        next_id: 0,
        tx_rows: HashMap::new(),
        rx_row: None,
        rx_wpm: tone.wpm,
        pending: 0,
    }));
    {
        let (ui, state) = (ui.clone(), state.clone());
        glib::spawn_future_local(async move {
            while let Ok(event) = receiver.recv().await {
                handle_event(&ui, &state, event);
            }
        });
    }

    // Header bar and settings.
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&window_title));
    header.pack_end(&settings_button(&state, &config, &ui));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(&title)
        .default_width(720)
        .default_height(640)
        .content(&toolbar)
        .build();

    // Sending.
    let submit = {
        let (ui, state) = (ui.clone(), state.clone());
        move || send_message(&ui, &state)
    };
    let submit = Rc::new(submit);
    entry.connect_activate({
        let submit = submit.clone();
        move |_| submit()
    });
    send.connect_clicked(move |_| submit());
    entry.connect_changed({
        let ui = ui.clone();
        move |entry| validate(&ui, &entry.text())
    });
    stop.connect_clicked({
        let state = state.clone();
        move |_| abort(&state)
    });
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let state = state.clone();
        move |_, key, _, _| {
            if key == gdk::Key::Escape {
                abort(&state);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });
    window.add_controller(keys);

    // Stop the PipeWire thread with the window.
    window.connect_close_request({
        let state = state.clone();
        move |_| {
            state.borrow_mut().engine.take();
            glib::Propagation::Proceed
        }
    });

    window.present();
    entry.grab_focus();
}

fn settings_button(
    state: &Rc<RefCell<State>>,
    config: &engine::Config,
    ui: &Ui,
) -> gtk::MenuButton {
    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let tone = state.borrow().tone.clone();
    let label = |text: &str| gtk::Label::builder().label(text).xalign(0.0).build();
    let heading = |text: &str| {
        gtk::Label::builder()
            .label(text)
            .xalign(0.0)
            .css_classes(["heading"])
            .build()
    };

    let wpm = gtk::SpinButton::with_range(5.0, 60.0, 1.0);
    wpm.set_value(tone.wpm);
    let tx_tone = gtk::SpinButton::with_range(300.0, 1500.0, 10.0);
    tx_tone.set_value(tone.tone);
    let gain = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
    gain.set_value(tone.gain);
    gain.set_hexpand(true);
    gain.set_draw_value(true);
    gain.set_digits(2);
    let rx = gtk::SpinButton::with_range(300.0, 1500.0, 10.0);
    rx.set_value(config.rx_tone);
    let mute = gtk::Switch::builder()
        .active(config.rx_mute)
        .halign(gtk::Align::Start)
        .build();

    grid.attach(&heading("Transmit"), 0, 0, 2, 1);
    grid.attach(&label("Speed (WPM)"), 0, 1, 1, 1);
    grid.attach(&wpm, 1, 1, 1, 1);
    grid.attach(&label("Tone (Hz)"), 0, 2, 1, 1);
    grid.attach(&tx_tone, 1, 2, 1, 1);
    grid.attach(&label("Gain"), 0, 3, 1, 1);
    grid.attach(&gain, 1, 3, 1, 1);
    grid.attach(&heading("Receive"), 0, 4, 2, 1);
    grid.attach(&label("Tone (Hz)"), 0, 5, 1, 1);
    grid.attach(&rx, 1, 5, 1, 1);
    grid.attach(&label("Mute while sending"), 0, 6, 1, 1);
    grid.attach(&mute, 1, 6, 1, 1);

    wpm.connect_value_changed({
        let state = state.clone();
        move |spin| state.borrow_mut().tone.wpm = spin.value()
    });
    tx_tone.connect_value_changed({
        let state = state.clone();
        move |spin| state.borrow_mut().tone.tone = spin.value()
    });
    gain.connect_value_changed({
        let state = state.clone();
        move |scale| state.borrow_mut().tone.gain = scale.value()
    });
    rx.connect_value_changed({
        let (state, ui) = (state.clone(), ui.clone());
        move |spin| {
            command(&state, Command::SetRxTone(spin.value()));
            ui.rx_info.set_label(&format!("RX {:.0} Hz", spin.value()));
        }
    });
    mute.connect_active_notify({
        let state = state.clone();
        move |switch| command(&state, Command::SetRxMute(switch.is_active()))
    });

    gtk::MenuButton::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Settings")
        .popover(&gtk::Popover::builder().child(&grid).build())
        .build()
}

fn command(state: &Rc<RefCell<State>>, command: Command) {
    if let Some(engine) = &state.borrow().engine {
        engine.send(command);
    }
}

fn abort(state: &Rc<RefCell<State>>) {
    command(state, Command::Abort);
}

fn validate(ui: &Ui, text: &str) {
    let result = if text.trim().is_empty() {
        Err(String::new())
    } else {
        encoder::encode(text).map(|_| ())
    };
    match &result {
        Err(message) if !message.is_empty() => {
            ui.entry.add_css_class("error");
            ui.entry.set_tooltip_text(Some(message));
        }
        _ => {
            ui.entry.remove_css_class("error");
            ui.entry.set_tooltip_text(None);
        }
    }
    ui.send.set_sensitive(result.is_ok());
}

fn send_message(ui: &Ui, state: &Rc<RefCell<State>>) {
    let text = ui.entry.text().trim().to_uppercase();
    if text.is_empty() {
        return;
    }
    let mut st = state.borrow_mut();
    if st.engine.is_none() {
        ui.toasts.add_toast(adw::Toast::new("Audio is unavailable"));
        return;
    }
    let (message, samples) = match render(&text, &st.tone) {
        Ok(rendered) => rendered,
        Err(error) => {
            ui.toasts.add_toast(adw::Toast::new(&error.to_string()));
            return;
        }
    };
    st.next_id += 1;
    let id = st.next_id;
    let details = format!("{} WPM · {:.0} Hz", st.tone.wpm, st.tone.tone);
    let (bubble, _, _) = bubble(true, &format!("TX · {} · {details}", now()), &text);
    let morse = gtk::Label::builder()
        .label(message.to_string())
        .xalign(0.0)
        .wrap(true)
        .selectable(true)
        .css_classes(["monospace", "caption", "dim-label"])
        .build();
    let progress = gtk::ProgressBar::builder().visible(false).build();
    let status = gtk::Label::builder()
        .label("Queued")
        .xalign(1.0)
        .css_classes(["caption", "dim-label"])
        .build();
    bubble.append(&morse);
    bubble.append(&progress);
    bubble.append(&status);
    close_rx(&mut st);
    append(ui, &bubble);
    st.tx_rows.insert(id, TxRow { status, progress });
    st.pending += 1;
    ui.stop.set_visible(true);
    if let Some(engine) = &st.engine {
        engine.send(Command::Send { id, samples });
    }
    ui.entry.set_text("");
}

fn handle_event(ui: &Ui, state: &Rc<RefCell<State>>, event: Event) {
    let mut st = state.borrow_mut();
    match event {
        Event::TxStarted(id) => {
            if let Some(row) = st.tx_rows.get(&id) {
                row.status.set_label("Sending");
                row.progress.set_visible(true);
            }
        }
        Event::TxProgress { id, fraction } => {
            if let Some(row) = st.tx_rows.get(&id) {
                row.progress.set_fraction(fraction);
            }
        }
        Event::TxDone(id) | Event::TxAborted(id) => {
            let done = matches!(event, Event::TxDone(_));
            if let Some(row) = st.tx_rows.remove(&id) {
                row.progress.set_visible(false);
                row.status.set_label(if done { "Sent" } else { "Stopped" });
            }
            st.pending = st.pending.saturating_sub(1);
            ui.stop.set_visible(st.pending > 0);
        }
        Event::Rx(RxEvent::Idle) => close_rx(&mut st),
        Event::Rx(event) => {
            if st.rx_row.is_none() {
                let (bubble, header, text) = bubble(false, &format!("RX · {}", now()), "");
                append(ui, &bubble);
                st.rx_row = Some(RxRow {
                    header,
                    text,
                    content: String::new(),
                });
            }
            let wpm = st.rx_wpm;
            let row = st.rx_row.as_mut().unwrap();
            match event {
                RxEvent::Char(c) => row.content.push(c),
                RxEvent::WordGap => row.content.push(' '),
                RxEvent::Idle => unreachable!(),
            }
            row.text.set_label(&row.content);
            if !row.header.label().contains("WPM") {
                row.header
                    .set_label(&format!("{} · ~{wpm:.0} WPM", row.header.label()));
            }
            scroll_to_end(ui);
        }
        Event::RxStatus(status) => show_status(ui, &mut st, status),
        Event::Info(message) => {
            if message.starts_with("Linked") {
                ui.toasts.add_toast(adw::Toast::new(&message));
            }
        }
        Event::Error(message) => ui.toasts.add_toast(adw::Toast::new(&message)),
        Event::Stopped => {
            if st.engine.is_some() {
                show_banner(ui, "Lost the PipeWire connection");
            }
        }
    }
}

fn show_status(ui: &Ui, st: &mut State, status: Status) {
    let db = 20.0 * status.level.max(1e-6).log10();
    ui.level
        .set_value(((db + 80.0) / 80.0).clamp(0.0, 1.0) as f64);
    if status.keyed {
        ui.key.set_css_classes(&["keyed"]);
    } else {
        ui.key.set_css_classes(&["idle-key"]);
    }
    st.rx_wpm = status.wpm;
    let tone = ui.rx_info.label();
    let tone = tone.split(" · ").next().unwrap_or("RX").to_owned();
    ui.rx_info
        .set_label(&format!("{tone} · {:.0} WPM", status.wpm));
}

/// Finish the current RX over, trimming the trailing word gap.
fn close_rx(st: &mut State) {
    if let Some(row) = st.rx_row.take() {
        row.text.set_label(row.content.trim_end());
    }
}

fn show_banner(ui: &Ui, message: &str) {
    ui.banner.set_title(message);
    ui.banner.set_revealed(true);
    ui.send.set_sensitive(false);
}

fn now() -> String {
    glib::DateTime::now_local()
        .and_then(|t| t.format("%H:%M:%S"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// A message bubble: sent overs on the right, received on the left.
fn bubble(tx: bool, header: &str, text: &str) -> (gtk::Box, gtk::Label, gtk::Label) {
    let bubble = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .halign(if tx {
            gtk::Align::End
        } else {
            gtk::Align::Start
        })
        .css_classes(["bubble", if tx { "tx" } else { "rx" }])
        .build();
    let header = gtk::Label::builder()
        .label(header)
        .xalign(0.0)
        .css_classes(["caption", "dim-label"])
        .build();
    let text = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .selectable(true)
        .max_width_chars(60)
        .css_classes(["message"])
        .build();
    bubble.append(&header);
    bubble.append(&text);
    (bubble, header, text)
}

fn append(ui: &Ui, widget: &impl IsA<gtk::Widget>) {
    ui.empty.set_visible(false);
    ui.scroller.set_visible(true);
    ui.transcript.append(widget);
    scroll_to_end(ui);
}

/// Keep the newest message in view once layout has caught up.
fn scroll_to_end(ui: &Ui) {
    let adjustment = ui.scroller.vadjustment();
    glib::idle_add_local_once(move || {
        adjustment.set_value(adjustment.upper() - adjustment.page_size());
    });
}
