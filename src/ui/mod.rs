//! GTK4/libadwaita chat window: a transcript of sent and received overs, an input
//! row, RX level and speed, and TX/RX settings.

use crate::{EngineArgs, PttArgs, ToneArgs, render};
use adw::prelude::*;
use cw_chat::{
    audio::engine::{self, Command, Engine, Event},
    morse::encoder,
    ptt::{self, Ptt, PttEvent},
    rx::decoder::{OverInfo, RxEvent, Status},
};
use gtk::{gdk, gio, glib};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

const APP_ID: &str = "net.cwchat.CwChat";

const CSS: &str = "
.bubble { padding: 8px 12px; border-radius: 12px; }
.bubble.tx { background-color: alpha(@accent_bg_color, 0.18); }
.bubble.rx { background-color: alpha(@view_fg_color, 0.07); }
.bubble.rx.station-1 { background-color: alpha(@success_bg_color, 0.2); }
.bubble.rx.station-2 { background-color: alpha(@warning_bg_color, 0.2); }
.bubble.rx.station-3 { background-color: alpha(@error_bg_color, 0.16); }
.bubble .message { font-size: 1.15em; }
.keyed { color: @success_color; }
.idle-key { color: alpha(@view_fg_color, 0.25); }
.on-air { font-weight: bold; padding: 2px 10px; border-radius: 6px;
          background-color: @error_bg_color; color: @error_fg_color; }
.on-air.keying { background-color: @warning_bg_color; color: @warning_fg_color; }
";

/// What the window hears from its background threads.
enum UiEvent {
    Engine(Event),
    Ptt(PttEvent),
}

pub fn run(
    engine: EngineArgs,
    tone: ToneArgs,
    ptt: PttArgs,
) -> Result<(), Box<dyn std::error::Error>> {
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
        app.set_accels_for_action("win.copy-all", &["<Ctrl><Shift>c"]);
        app.set_accels_for_action("win.clear", &["<Ctrl>l"]);
        app.set_accels_for_action("win.new-line", &["<Ctrl>Return"]);
    });
    app.connect_activate(move |app| build(app, engine.clone(), tone.clone(), ptt.clone()));
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
    bubble: gtk::Box,
    header: gtk::Label,
    text: gtk::Label,
    content: String,
    time: String,
    /// The sender's speed has been added to the header.
    has_wpm: bool,
}

struct State {
    engine: Option<Engine>,
    /// Sends messages to the engine, keying the radio around them when enabled.
    ptt: Option<Ptt>,
    /// Transmit on the tone RX is listening at (zero-beat with the other station).
    tx_follows_rx: bool,
    rx_tone: f64,
    tone: ToneArgs,
    next_id: u64,
    tx_rows: HashMap<u64, TxRow>,
    rx_row: Option<RxRow>,
    /// The over being received: who is sending, for the bubble header.
    rx_over: Option<OverInfo>,
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
    tone: gtk::Scale,
    tone_label: gtk::Label,
    auto: gtk::ToggleButton,
    /// Set while the window moves the tone controls itself, so their handlers
    /// do not treat it as the user taking over.
    syncing: Rc<std::cell::Cell<bool>>,
    toasts: adw::ToastOverlay,
    banner: adw::Banner,
    on_air: gtk::Label,
}

fn build(app: &adw::Application, engine_args: EngineArgs, tone: ToneArgs, ptt_args: PttArgs) {
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
            "Type below to send CW. Received CW appears here as it is decoded.\n{}",
            if config.rx_auto {
                "The RX tone follows the signal automatically.".to_owned()
            } else {
                format!("RX listens at {:.0} Hz.", config.rx_tone)
            }
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
        .label(format!("{:.0} Hz", config.rx_tone))
        .css_classes(["caption", "dim-label", "numeric"])
        .selectable(true)
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
    let new_line = gtk::Button::builder()
        .label("New line")
        .css_classes(["flat"])
        .tooltip_text("Start a new RX line now (Ctrl+Enter)")
        .build();
    meter.append(&key);
    meter.append(&level);
    meter.append(&rx_info);
    meter.append(&new_line);
    meter.append(&stop);

    // RX tone row: live slider plus automatic tracking.
    let tone_scale = gtk::Scale::with_range(
        gtk::Orientation::Horizontal,
        cw_chat::rx::tuner::MIN_HZ,
        cw_chat::rx::tuner::MAX_HZ,
        5.0,
    );
    tone_scale.set_value(config.rx_tone);
    tone_scale.set_hexpand(true);
    tone_scale.set_tooltip_text(Some(
        "Audio pitch the decoder listens for; dragging switches to manual",
    ));
    let tone_label = gtk::Label::builder()
        .label(format!("{:.0} Hz", config.rx_tone))
        .css_classes(["numeric"])
        .selectable(true)
        .width_chars(8)
        .xalign(1.0)
        .build();
    let auto = gtk::ToggleButton::builder()
        .label("Auto")
        .active(config.rx_auto)
        .tooltip_text("Follow the strongest CW signal between 300 and 1200 Hz")
        .build();
    let tone_row = gtk::Box::builder()
        .spacing(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    tone_row.append(
        &gtk::Label::builder()
            .label("Tone")
            .css_classes(["caption-heading"])
            .build(),
    );
    tone_row.append(&tone_scale);
    tone_row.append(&tone_label);
    tone_row.append(&auto);

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
    content.append(&tone_row);
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
        tone: tone_scale.clone(),
        tone_label,
        auto: auto.clone(),
        syncing: Rc::new(std::cell::Cell::new(false)),
        toasts: toasts.clone(),
        banner,
        on_air: gtk::Label::builder()
            .css_classes(["on-air"])
            .visible(false)
            .tooltip_text("The radio is keyed through HRD")
            .build(),
    };

    // Engine and push-to-talk, with events forwarded to this thread. Finished
    // messages go straight to the PTT controller so unkeying never waits on GTK.
    let (sender, receiver) = async_channel::unbounded();
    let (ptt_link, ptt_inbox) = ptt::link();
    let engine_sender = sender.clone();
    let engine = match Engine::start(config.clone(), move |event| {
        if let Event::TxDone(id) | Event::TxAborted(id) = event {
            ptt_link.finished(id);
        }
        let _ = engine_sender.try_send(UiEvent::Engine(event));
    }) {
        Ok(engine) => Some(engine),
        Err(error) => {
            show_banner(&ui, &format!("Audio unavailable: {error}"));
            None
        }
    };
    let ptt_available = ptt_args.ptt.is_some();
    let ptt = engine.as_ref().map(|engine| {
        Ptt::start(
            ptt_inbox,
            ptt_args.keyer(),
            ptt_available,
            ptt_args.config(),
            engine.sender(),
            move |event| {
                let _ = sender.try_send(UiEvent::Ptt(event));
            },
        )
    });
    let state = Rc::new(RefCell::new(State {
        engine,
        ptt,
        tx_follows_rx: true,
        rx_tone: config.rx_tone,
        tone: tone.clone(),
        next_id: 0,
        tx_rows: HashMap::new(),
        rx_row: None,
        rx_over: None,
        rx_wpm: tone.wpm,
        pending: 0,
    }));
    {
        let (ui, state) = (ui.clone(), state.clone());
        glib::spawn_future_local(async move {
            while let Ok(event) = receiver.recv().await {
                match event {
                    UiEvent::Engine(event) => handle_event(&ui, &state, event),
                    UiEvent::Ptt(event) => handle_ptt(&ui, &state, event),
                }
            }
        });
    }

    // Header bar and settings.
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&window_title));
    let clear = gtk::Button::builder()
        .icon_name("edit-clear-all-symbolic")
        .tooltip_text("Clear the transcript (Ctrl+L)")
        .build();
    header.pack_start(&clear);
    header.pack_end(&settings_button(&state, &config, ptt_available));
    header.pack_end(&ui.on_air);
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
    tone_scale.connect_value_changed({
        let (ui, state) = (ui.clone(), state.clone());
        move |scale| {
            ui.tone_label.set_label(&format!("{:.0} Hz", scale.value()));
            if ui.syncing.get() {
                return;
            }
            // The user took over: fixed tone from here on.
            if ui.auto.is_active() {
                ui.syncing.set(true);
                ui.auto.set_active(false);
                ui.syncing.set(false);
                command(&state, Command::SetRxAuto(false));
            }
            command(&state, Command::SetRxTone(scale.value()));
        }
    });
    auto.connect_toggled({
        let (ui, state) = (ui.clone(), state.clone());
        move |auto| {
            if ui.syncing.get() {
                return;
            }
            command(&state, Command::SetRxAuto(auto.is_active()));
            if !auto.is_active() {
                command(&state, Command::SetRxTone(ui.tone.value()));
            }
        }
    });
    new_line.set_action_name(Some("win.new-line"));
    clear.set_action_name(Some("win.clear"));
    add_action(&window, "new-line", {
        let state = state.clone();
        move || close_rx(&mut state.borrow_mut())
    });
    add_action(&window, "clear", {
        let (ui, state) = (ui.clone(), state.clone());
        move || clear_transcript(&ui, &state)
    });
    add_action(&window, "copy-all", {
        let ui = ui.clone();
        move || copy(&ui, &transcript_text(&ui))
    });
    for area in [
        ui.scroller.upcast_ref::<gtk::Widget>(),
        ui.empty.upcast_ref(),
    ] {
        area.add_controller(context_menu_gesture());
    }
    // Capture phase, so Ctrl+Enter reaches us before the entry treats it as Send.
    // (Ctrl+L and Ctrl+Shift+C are application accelerators.)
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let state = state.clone();
        move |_, key, _, modifiers| {
            let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
            match key {
                gdk::Key::Escape => abort(&state),
                gdk::Key::Return | gdk::Key::KP_Enter if ctrl => close_rx(&mut state.borrow_mut()),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        }
    });
    window.add_controller(keys);

    // Unkey (PTT first), then stop the PipeWire thread, with the window.
    window.connect_close_request({
        let state = state.clone();
        move |_| {
            let (ptt, engine) = {
                let mut st = state.borrow_mut();
                (st.ptt.take(), st.engine.take())
            };
            drop(ptt);
            drop(engine);
            glib::Propagation::Proceed
        }
    });

    window.present();
    entry.grab_focus();
}

fn settings_button(
    state: &Rc<RefCell<State>>,
    config: &engine::Config,
    ptt_available: bool,
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
    let follow = gtk::Switch::builder()
        .active(state.borrow().tx_follows_rx)
        .halign(gtk::Align::Start)
        .tooltip_text(
            "Send on the pitch RX is listening at, to answer on the other station's frequency",
        )
        .build();
    tx_tone.set_sensitive(!follow.is_active());
    let key_radio = gtk::Switch::builder()
        .active(ptt_available)
        .sensitive(ptt_available)
        .halign(gtk::Align::Start)
        .tooltip_text(if ptt_available {
            "Key the transmitter through HRD around each message"
        } else {
            "Start cw-chat with --ptt hrdctl to key the radio"
        })
        .build();
    let gain = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
    gain.set_value(tone.gain);
    gain.set_hexpand(true);
    gain.set_draw_value(true);
    gain.set_digits(2);
    let mute = gtk::Switch::builder()
        .active(config.rx_mute)
        .halign(gtk::Align::Start)
        .build();

    grid.attach(&heading("Transmit"), 0, 0, 2, 1);
    grid.attach(&label("Speed (WPM)"), 0, 1, 1, 1);
    grid.attach(&wpm, 1, 1, 1, 1);
    grid.attach(&label("Tone (Hz)"), 0, 2, 1, 1);
    grid.attach(&tx_tone, 1, 2, 1, 1);
    grid.attach(&label("TX tone follows RX tone"), 0, 3, 1, 1);
    grid.attach(&follow, 1, 3, 1, 1);
    grid.attach(&label("Gain"), 0, 4, 1, 1);
    grid.attach(&gain, 1, 4, 1, 1);
    grid.attach(&label("Key the radio (PTT)"), 0, 5, 1, 1);
    grid.attach(&key_radio, 1, 5, 1, 1);
    grid.attach(&heading("Receive"), 0, 6, 2, 1);
    grid.attach(&label("Mute while sending"), 0, 7, 1, 1);
    grid.attach(&mute, 1, 7, 1, 1);

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
    follow.connect_active_notify({
        let (state, tx_tone) = (state.clone(), tx_tone.clone());
        move |switch| {
            state.borrow_mut().tx_follows_rx = switch.is_active();
            tx_tone.set_sensitive(!switch.is_active());
        }
    });
    key_radio.connect_active_notify({
        let state = state.clone();
        move |switch| {
            if let Some(ptt) = &state.borrow().ptt {
                ptt.set_enabled(switch.is_active());
            }
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

/// Stop sending and clear the queue; the PTT controller also unkeys.
fn abort(state: &Rc<RefCell<State>>) {
    match &state.borrow().ptt {
        Some(ptt) => ptt.abort(),
        None => command(state, Command::Abort),
    }
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
    if st.ptt.is_none() {
        ui.toasts.add_toast(adw::Toast::new("Audio is unavailable"));
        return;
    }
    let mut tone = st.tone.clone();
    if st.tx_follows_rx {
        tone.tone = st.rx_tone;
    }
    let (message, samples) = match render(&text, &tone) {
        Ok(rendered) => rendered,
        Err(error) => {
            ui.toasts.add_toast(adw::Toast::new(&error.to_string()));
            return;
        }
    };
    st.next_id += 1;
    let id = st.next_id;
    let details = format!("{} WPM · {:.0} Hz", tone.wpm, tone.tone);
    let (bubble, _, _) = bubble(ui, true, &format!("TX · {} · {details}", now()), &text);
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
        .selectable(true)
        .css_classes(["caption", "dim-label"])
        .build();
    for label in [&morse, &status] {
        label.set_extra_menu(Some(&context_menu(true)));
    }
    bubble.append(&morse);
    bubble.append(&progress);
    bubble.append(&status);
    close_rx(&mut st);
    append(ui, &bubble);
    st.tx_rows.insert(id, TxRow { status, progress });
    st.pending += 1;
    ui.stop.set_visible(true);
    if let Some(ptt) = &st.ptt {
        ptt.submit(id, samples);
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
        Event::TxDone(id) => finish_tx(ui, &mut st, id, true),
        Event::TxAborted(id) => finish_tx(ui, &mut st, id, false),
        Event::Rx(RxEvent::Idle) => close_rx(&mut st),
        Event::Rx(RxEvent::Over(info)) => {
            close_rx(&mut st);
            st.rx_over = Some(info);
        }
        Event::Rx(RxEvent::OverUpdate(info)) => {
            let previous = st.rx_over.replace(info);
            let wpm = st.rx_wpm;
            if let Some(row) = &st.rx_row {
                if let Some(previous) = previous {
                    row.bubble
                        .remove_css_class(&format!("station-{}", previous.station % 4));
                }
                row.bubble
                    .add_css_class(&format!("station-{}", info.station % 4));
                row.header
                    .set_label(&rx_title(Some(info), &row.time, row.has_wpm.then_some(wpm)));
            }
        }
        // A word gap right after a forced new line would only add a leading space.
        Event::Rx(RxEvent::WordGap) if st.rx_row.is_none() => {}
        Event::Rx(event) => {
            if st.rx_row.is_none() {
                let time = now();
                let title = rx_title(st.rx_over, &time, None);
                let (bubble, header, text) = bubble(ui, false, &title, "");
                if let Some(over) = st.rx_over {
                    bubble.add_css_class(&format!("station-{}", over.station % 4));
                }
                append(ui, &bubble);
                st.rx_row = Some(RxRow {
                    bubble,
                    header,
                    text,
                    content: String::new(),
                    time,
                    has_wpm: false,
                });
            }
            let (wpm, over) = (st.rx_wpm, st.rx_over);
            let row = st.rx_row.as_mut().unwrap();
            match event {
                RxEvent::Char(c) => row.content.push(c),
                RxEvent::WordGap => row.content.push(' '),
                RxEvent::Idle | RxEvent::Over(_) | RxEvent::OverUpdate(_) => unreachable!(),
            }
            row.text.set_label(&row.content);
            if !row.has_wpm {
                row.has_wpm = true;
                row.header.set_label(&rx_title(over, &row.time, Some(wpm)));
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

fn finish_tx(ui: &Ui, st: &mut State, id: u64, sent: bool) {
    if let Some(row) = st.tx_rows.remove(&id) {
        row.progress.set_visible(false);
        row.status.set_label(if sent { "Sent" } else { "Stopped" });
    }
    st.pending = st.pending.saturating_sub(1);
    ui.stop.set_visible(st.pending > 0);
}

fn handle_ptt(ui: &Ui, state: &Rc<RefCell<State>>, event: PttEvent) {
    let mut st = state.borrow_mut();
    match event {
        PttEvent::Keying => {
            ui.on_air.set_label("KEYING");
            ui.on_air.add_css_class("keying");
            ui.on_air.set_visible(true);
        }
        PttEvent::Keyed => {
            ui.on_air.set_label("ON AIR");
            ui.on_air.remove_css_class("keying");
            ui.on_air.set_visible(true);
        }
        PttEvent::Unkeyed => {
            ui.on_air.set_visible(false);
        }
        PttEvent::MessageAborted(id) => finish_tx(ui, &mut st, id, false),
        PttEvent::Error(message) => {
            if message.starts_with("Could not unkey") {
                ui.on_air.set_label("ON AIR · unkey failed, retrying");
                ui.on_air.remove_css_class("keying");
                ui.on_air.set_visible(true);
            }
            ui.toasts.add_toast(adw::Toast::new(&message));
        }
    }
}

fn show_status(ui: &Ui, st: &mut State, status: Status) {
    st.rx_tone = status.tone;
    let db = 20.0 * status.level.max(1e-6).log10();
    ui.level
        .set_value(((db + 80.0) / 80.0).clamp(0.0, 1.0) as f64);
    if status.keyed {
        ui.key.set_css_classes(&["keyed"]);
    } else {
        ui.key.set_css_classes(&["idle-key"]);
    }
    st.rx_wpm = status.wpm;
    let label = format!("{:.0} Hz · {:.0} WPM", status.tone, status.wpm);
    if ui.rx_info.label() != label {
        ui.rx_info.set_label(&label);
    }
    // In auto mode the slider shows where the decoder has tuned.
    if status.auto && (ui.tone.value() - status.tone).abs() >= 1.0 {
        ui.syncing.set(true);
        ui.tone.set_value(status.tone);
        ui.syncing.set(false);
    }
}

/// Finish the current RX over, trimming the trailing word gap.
fn close_rx(st: &mut State) {
    if let Some(row) = st.rx_row.take() {
        row.text.set_label(row.content.trim_end());
    }
}

/// Remove every message; sending and decoding carry on, and later text starts fresh.
fn clear_transcript(ui: &Ui, state: &Rc<RefCell<State>>) {
    close_rx(&mut state.borrow_mut());
    while let Some(child) = ui.transcript.first_child() {
        ui.transcript.remove(&child);
    }
    ui.scroller.set_visible(false);
    ui.empty.set_visible(true);
}

fn show_banner(ui: &Ui, message: &str) {
    ui.banner.set_title(message);
    ui.banner.set_revealed(true);
    ui.send.set_sensitive(false);
}

/// Header for an RX bubble: station, time, pitch, level, then speed once known.
fn rx_title(over: Option<OverInfo>, time: &str, wpm: Option<f64>) -> String {
    let mut title = match over {
        Some(over) => format!(
            "RX · Station {} · {time} · {:.0} Hz · {:.0} dBFS",
            station_letter(over.station),
            over.pitch_hz,
            over.level_db
        ),
        None => format!("RX · {time}"),
    };
    if let Some(wpm) = wpm {
        title.push_str(&format!(" · ~{wpm:.0} WPM"));
    }
    title
}

/// Stations are lettered in the order they are first heard: A, B, ... Z, A2, ...
fn station_letter(station: usize) -> String {
    let letter = (b'A' + (station % 26) as u8) as char;
    match station / 26 {
        0 => letter.to_string(),
        round => format!("{letter}{}", round + 1),
    }
}

fn now() -> String {
    glib::DateTime::now_local()
        .and_then(|t| t.format("%H:%M:%S"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// A message bubble: sent overs on the right, received on the left. Every label is
/// selectable and its context menu adds Copy Message, Copy All, New Line, and Clear.
fn bubble(ui: &Ui, tx: bool, header: &str, text: &str) -> (gtk::Box, gtk::Label, gtk::Label) {
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
        .selectable(true)
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
    for label in [&header, &text] {
        label.set_extra_menu(Some(&context_menu(true)));
    }
    bubble.append(&header);
    bubble.append(&text);
    // "msg.copy" resolves through the widget tree, so it copies this bubble.
    let actions = gio::SimpleActionGroup::new();
    let copy_message = gio::SimpleAction::new("copy", None);
    copy_message.connect_activate({
        let (ui, bubble) = (ui.clone(), bubble.downgrade());
        move |_, _| {
            if let Some(bubble) = bubble.upgrade() {
                copy(&ui, &bubble_text(&bubble));
            }
        }
    });
    actions.add_action(&copy_message);
    bubble.insert_action_group("msg", Some(&actions));
    (bubble, header, text)
}

fn add_action(window: &adw::ApplicationWindow, name: &str, activate: impl Fn() + 'static) {
    let action = gio::SimpleAction::new(name, None);
    action.connect_activate(move |_, _| activate());
    window.add_action(&action);
}

/// Items for the transcript context menu; Copy Message only applies over a bubble.
fn context_menu(message: bool) -> gio::Menu {
    let copy = gio::Menu::new();
    if message {
        copy.append(Some("Copy Message"), Some("msg.copy"));
    }
    copy.append(Some("Copy All"), Some("win.copy-all"));
    let edit = gio::Menu::new();
    edit.append(Some("New Line"), Some("win.new-line"));
    edit.append(Some("Clear"), Some("win.clear"));
    let menu = gio::Menu::new();
    menu.append_section(None, &copy);
    menu.append_section(None, &edit);
    menu
}

/// Right-click outside any label: selectable labels show their own menu (with our
/// items appended), so this covers bubble backgrounds and empty space.
fn context_menu_gesture() -> gtk::GestureClick {
    let gesture = gtk::GestureClick::builder()
        .button(gdk::BUTTON_SECONDARY)
        .build();
    gesture.connect_pressed(|gesture, _, x, y| {
        let Some(area) = gesture.widget() else {
            return;
        };
        let picked = area.pick(x, y, gtk::PickFlags::DEFAULT);
        let mut bubble = None;
        let mut widget = picked;
        while let Some(current) = widget {
            if current.is::<gtk::Label>() {
                return;
            }
            if current.has_css_class("bubble") {
                bubble = Some(current);
                break;
            }
            if current == area {
                break;
            }
            widget = current.parent();
        }
        let parent = bubble.clone().unwrap_or_else(|| area.clone());
        let popover = gtk::PopoverMenu::from_model(Some(&context_menu(bubble.is_some())));
        popover.set_parent(&parent);
        popover.set_has_arrow(false);
        popover.set_halign(gtk::Align::Start);
        let point = area
            .compute_point(&parent, &gtk::graphene::Point::new(x as f32, y as f32))
            .unwrap_or_else(|| gtk::graphene::Point::new(x as f32, y as f32));
        popover.set_pointing_to(Some(&gdk::Rectangle::new(
            point.x() as i32,
            point.y() as i32,
            1,
            1,
        )));
        popover.connect_closed(|popover| {
            // Unparent after the close animation has let go of the popover.
            let popover = popover.clone();
            glib::idle_add_local_once(move || popover.unparent());
        });
        gesture.set_state(gtk::EventSequenceState::Claimed);
        popover.popup();
    });
    gesture
}

/// A bubble's visible labels, one per line: header, text, Morse, status.
fn bubble_text(bubble: &gtk::Box) -> String {
    let mut lines = Vec::new();
    let mut child = bubble.first_child();
    while let Some(widget) = child {
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            let text = label.text();
            if widget.is_visible() && !text.is_empty() {
                lines.push(text.to_string());
            }
        }
        child = widget.next_sibling();
    }
    lines.join("\n")
}

fn transcript_text(ui: &Ui) -> String {
    let mut messages = Vec::new();
    let mut child = ui.transcript.first_child();
    while let Some(widget) = child {
        if let Some(bubble) = widget.downcast_ref::<gtk::Box>() {
            messages.push(bubble_text(bubble));
        }
        child = widget.next_sibling();
    }
    messages.join("\n\n")
}

fn copy(ui: &Ui, text: &str) {
    if text.is_empty() {
        ui.toasts.add_toast(adw::Toast::new("Nothing to copy"));
        return;
    }
    ui.transcript.clipboard().set_text(text);
    ui.toasts.add_toast(adw::Toast::new("Copied to clipboard"));
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
