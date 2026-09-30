//! Long-lived PipeWire engine for the chat window: one TX playback node and one
//! RX capture node, run on a dedicated thread with its own main loop.

use super::pipewire::format_pod;
use crate::{
    cw::oscillator::SAMPLE_RATE,
    rx::decoder::{Decoder, RxEvent, Status},
};
use pipewire::{self as pw, properties::properties, spa};
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::Rc,
    sync::mpsc,
    thread::JoinHandle,
};

type Error = Box<dyn std::error::Error>;

#[derive(Debug, Clone)]
pub struct Config {
    /// Instance name; nodes are `cw-chat-<name>-tx` and `cw-chat-<name>-rx`.
    pub name: Option<String>,
    /// PipeWire target (node.name or object.serial) for TX playback.
    pub tx_target: Option<String>,
    /// PipeWire target for RX capture, routed by the session manager.
    pub rx_target: Option<String>,
    /// Node to link into RX directly: a node.name, object.serial, or another
    /// instance's name. Linked whenever it appears, without session manager policy.
    pub rx_from: Option<String>,
    /// Leave both nodes unconnected, for manual routing in qpwgraph.
    pub manual: bool,
    /// RX tone, or the starting point when `rx_auto` follows the signal.
    pub rx_tone: f64,
    pub rx_auto: bool,
    pub rx_wpm_hint: f64,
    /// Ignore RX while transmitting, so our own audio is not decoded.
    pub rx_mute: bool,
}

impl Config {
    pub fn node_name(&self, direction: &str) -> String {
        match &self.name {
            Some(name) => format!("cw-chat-{name}-{direction}"),
            None => format!("cw-chat-{direction}"),
        }
    }
}

#[derive(Debug)]
pub enum Command {
    /// Queue rendered audio; it should end with a word gap so queued messages stay separate.
    Send {
        id: u64,
        samples: Vec<f32>,
    },
    /// Stop the current message and clear the queue.
    Abort,
    SetRxTone(f64),
    SetRxAuto(bool),
    SetRxMute(bool),
    Quit,
}

#[derive(Debug, Clone)]
pub enum Event {
    TxStarted(u64),
    TxProgress {
        id: u64,
        fraction: f64,
    },
    TxDone(u64),
    TxAborted(u64),
    Rx(RxEvent),
    RxStatus(Status),
    Info(String),
    /// A problem that leaves the engine running, such as a stream PipeWire could not route.
    Error(String),
    /// The engine thread has ended, after a Quit command or a lost PipeWire connection.
    Stopped,
}

pub struct Engine {
    commands: pw::channel::Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl Engine {
    /// Start the PipeWire thread; `events` is called on that thread.
    pub fn start(config: Config, events: impl Fn(Event) + Send + 'static) -> Result<Self, Error> {
        let (commands, receiver) = pw::channel::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || {
                // Callbacks run on this thread only; share the sink by reference counting.
                let events: Rc<dyn Fn(Event)> = Rc::new(events);
                if let Err(error) = run(config, receiver, events.clone(), &ready_tx) {
                    // Startup failures are returned by `start`; later ones become events.
                    if ready_tx.send(Err(error.to_string())).is_err() {
                        events(Event::Error(error.to_string()));
                    }
                }
                events(Event::Stopped);
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                commands,
                thread: Some(thread),
            }),
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(message.into())
            }
            Err(_) => Err("PipeWire thread exited during startup".into()),
        }
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    /// A handle for sending commands from another thread (e.g. push-to-talk).
    pub fn sender(&self) -> CommandSender {
        CommandSender(self.commands.clone())
    }
}

/// Cloneable, thread-safe handle for sending commands to a running engine.
#[derive(Clone)]
pub struct CommandSender(pw::channel::Sender<Command>);

impl CommandSender {
    pub fn send(&self, command: Command) {
        let _ = self.0.send(command);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Quit);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Seconds of RX kept muted after a transmission ends, covering output latency.
const MUTE_TAIL_SECONDS: f64 = 0.25;
/// Interval between RX status and TX progress events.
const REPORT_SECONDS: f64 = 0.05;

struct Tx {
    queue: VecDeque<(u64, Vec<f32>)>,
    current: Option<(u64, Vec<f32>, usize)>,
    since_report: usize,
}

struct Rx {
    decoder: Decoder,
    mute: bool,
    /// RX samples still to discard; set while transmitting and for a tail afterwards.
    muted_samples: usize,
    since_report: usize,
}

fn run(
    config: Config,
    receiver: pw::channel::Receiver<Command>,
    events: Rc<dyn Fn(Event)>,
    ready: &mpsc::Sender<Result<(), String>>,
) -> Result<(), Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context
        .connect_rc(None)
        .map_err(|e| format!("Could not connect to PipeWire ({e}); is it running?"))?;

    let quit = mainloop.clone();
    let sink = events.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |_, _, _, message| {
            sink(Event::Error(format!("PipeWire: {message}")));
            quit.quit();
        })
        .register();

    let tx = Rc::new(RefCell::new(Tx {
        queue: VecDeque::new(),
        current: None,
        since_report: 0,
    }));
    let rx = Rc::new(RefCell::new(Rx {
        decoder: Decoder::new(SAMPLE_RATE, config.rx_tone, config.rx_wpm_hint)
            .with_auto(config.rx_auto),
        mute: config.rx_mute,
        muted_samples: 0,
        since_report: 0,
    }));
    let report_samples = (SAMPLE_RATE as f64 * REPORT_SECONDS) as usize;
    let tail_samples = (SAMPLE_RATE as f64 * MUTE_TAIL_SECONDS) as usize;

    // TX: always running; writes silence when idle so the node stays in the graph.
    let tx_name = config.node_name("tx");
    let mut tx_props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Communication",
        *pw::keys::NODE_NAME => tx_name.as_str(),
        *pw::keys::NODE_DESCRIPTION => format!("cw-chat TX {}", config.name.as_deref().unwrap_or("")).trim_end(),
        *pw::keys::AUDIO_CHANNELS => "1",
        "node.rate" => "1/48000",
    };
    if let Some(target) = &config.tx_target {
        tx_props.insert("target.object", target.as_str());
    }
    let tx_stream = pw::stream::StreamBox::new(&core, &tx_name, tx_props)?;
    let sink = events.clone();
    let tx_state = tx.clone();
    let rx_state = rx.clone();
    let _tx_listener = tx_stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let sink = events.clone();
            move |_, _, _, state| {
                if let pw::stream::StreamState::Error(message) = state {
                    sink(Event::Error(format!("TX stream: {message}")));
                }
            }
        })
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested() as usize;
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let mut tx = tx_state.borrow_mut();
            let mut written = 0;
            if let Some(bytes) = data.data() {
                let capacity = bytes.len() / 4;
                let frames = if requested == 0 {
                    capacity.min(1024)
                } else {
                    capacity.min(requested)
                };
                let out = &mut bytes[..frames * 4].as_chunks_mut::<4>().0;
                while written < frames {
                    if tx.current.is_none() {
                        let Some((id, samples)) = tx.queue.pop_front() else {
                            break;
                        };
                        sink(Event::TxStarted(id));
                        tx.current = Some((id, samples, 0));
                    }
                    let (id, samples, cursor) = tx.current.as_mut().unwrap();
                    let count = (frames - written).min(samples.len() - *cursor);
                    for (slot, sample) in out[written..written + count]
                        .iter_mut()
                        .zip(&samples[*cursor..*cursor + count])
                    {
                        slot.copy_from_slice(&sample.to_le_bytes());
                    }
                    *cursor += count;
                    written += count;
                    let (id, done, fraction) = (
                        *id,
                        *cursor == samples.len(),
                        *cursor as f64 / samples.len() as f64,
                    );
                    tx.since_report += count;
                    if done {
                        tx.current = None;
                        sink(Event::TxDone(id));
                    } else if tx.since_report >= report_samples {
                        tx.since_report = 0;
                        sink(Event::TxProgress { id, fraction });
                    }
                }
                for slot in &mut out[written..frames] {
                    slot.copy_from_slice(&0f32.to_le_bytes());
                }
                written = frames;
            }
            if tx.current.is_some() || !tx.queue.is_empty() {
                rx_state.borrow_mut().muted_samples = usize::MAX;
            } else {
                let mut rx = rx_state.borrow_mut();
                if rx.muted_samples == usize::MAX {
                    rx.muted_samples = tail_samples;
                }
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = 4;
            *chunk.size_mut() = (written * 4) as u32;
        })
        .register()?;

    // RX: capture, decode, and report.
    let rx_name = config.node_name("rx");
    let mut rx_props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Communication",
        *pw::keys::NODE_NAME => rx_name.as_str(),
        *pw::keys::NODE_DESCRIPTION => format!("cw-chat RX {}", config.name.as_deref().unwrap_or("")).trim_end(),
        *pw::keys::AUDIO_CHANNELS => "1",
        "node.rate" => "1/48000",
    };
    if let Some(target) = &config.rx_target {
        rx_props.insert("target.object", target.as_str());
    }
    let rx_stream = pw::stream::StreamBox::new(&core, &rx_name, rx_props)?;
    let sink = events.clone();
    let rx_state = rx.clone();
    let mut decoded = Vec::new();
    let mut samples = Vec::new();
    let _rx_listener = rx_stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let sink = events.clone();
            move |_, _, _, state| {
                if let pw::stream::StreamState::Error(message) = state {
                    sink(Event::Error(format!("RX stream: {message}")));
                }
            }
        })
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
            let Some(bytes) = data.data() else {
                return;
            };
            let end = (offset + size).min(bytes.len());
            samples.clear();
            samples.extend(
                bytes[offset.min(end)..end]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| f32::from_le_bytes(*b)),
            );
            let rx = &mut *rx_state.borrow_mut();
            if rx.mute && rx.muted_samples > 0 {
                rx.decoder.interrupt(&mut decoded);
                if rx.muted_samples != usize::MAX {
                    rx.muted_samples = rx.muted_samples.saturating_sub(samples.len());
                }
            } else {
                rx.muted_samples = 0;
                rx.decoder.process(&samples, &mut decoded);
            }
            for event in decoded.drain(..) {
                sink(Event::Rx(event));
            }
            rx.since_report += samples.len();
            if rx.since_report >= report_samples {
                rx.since_report = 0;
                sink(Event::RxStatus(rx.decoder.status()));
            }
        })
        .register()?;

    // Commands from the UI thread.
    let quit = mainloop.clone();
    let tx_state = tx.clone();
    let rx_state = rx.clone();
    let sink = events.clone();
    let _receiver = receiver.attach(mainloop.loop_(), move |command| match command {
        Command::Send { id, samples } => {
            if samples.is_empty() {
                sink(Event::TxDone(id));
            } else {
                tx_state.borrow_mut().queue.push_back((id, samples));
            }
        }
        Command::Abort => {
            let mut tx = tx_state.borrow_mut();
            let current = tx.current.take().map(|(id, _, _)| id);
            for id in current
                .into_iter()
                .chain(tx.queue.drain(..).map(|(id, _)| id))
            {
                sink(Event::TxAborted(id));
            }
        }
        Command::SetRxTone(tone) => rx_state.borrow_mut().decoder.set_frequency(tone),
        Command::SetRxAuto(auto) => rx_state.borrow_mut().decoder.set_auto(auto),
        Command::SetRxMute(mute) => rx_state.borrow_mut().mute = mute,
        Command::Quit => quit.quit(),
    });

    // Optional direct link into RX from another node, made whenever both exist.
    let _linker = match &config.rx_from {
        Some(source) => Some(Linker::start(
            &core,
            source.clone(),
            rx_name.clone(),
            events.clone(),
        )?),
        None => None,
    };

    let bytes = format_pod()?;
    let autoconnect = !config.manual;
    let flags = |auto: bool| {
        let mut flags = pw::stream::StreamFlags::MAP_BUFFERS;
        if auto {
            flags |= pw::stream::StreamFlags::AUTOCONNECT;
        }
        flags
    };
    let mut params = [spa::pod::Pod::from_bytes(&bytes).ok_or("Invalid audio format")?];
    tx_stream.connect(
        spa::utils::Direction::Output,
        None,
        flags(autoconnect),
        &mut params,
    )?;
    let mut params = [spa::pod::Pod::from_bytes(&bytes).ok_or("Invalid audio format")?];
    rx_stream.connect(
        spa::utils::Direction::Input,
        None,
        flags(autoconnect && config.rx_from.is_none()),
        &mut params,
    )?;

    let _ = ready.send(Ok(()));
    events(Event::Info(format!("PipeWire nodes: {tx_name}, {rx_name}")));
    mainloop.run();
    Ok(())
}

/// Watches the registry and links every output port of a source node into our RX
/// node's input, relinking if the source restarts.
struct Linker {
    _registry: pw::registry::RegistryRc,
    _listener: pw::registry::Listener,
}

#[derive(Default)]
struct Graph {
    /// Node id -> (node.name, object.serial).
    nodes: HashMap<u32, (String, String)>,
    /// Port id -> (node id, is output).
    ports: HashMap<u32, (u32, bool)>,
    links: HashMap<(u32, u32), pw::link::Link>,
}

impl Linker {
    fn start(
        core: &pw::core::CoreRc,
        source: String,
        rx_name: String,
        events: Rc<dyn Fn(Event)>,
    ) -> Result<Self, Error> {
        let registry = core.get_registry_rc()?;
        let graph = Rc::new(RefCell::new(Graph::default()));
        let instance_node = format!("cw-chat-{source}-tx");
        let update = {
            let graph = graph.clone();
            let core = core.clone();
            move || {
                let mut graph = graph.borrow_mut();
                let find = |matches: &dyn Fn(&(String, String)) -> bool| {
                    graph
                        .nodes
                        .iter()
                        .find(|(_, node)| matches(node))
                        .map(|(&id, _)| id)
                };
                let Some(rx) = find(&|(name, _)| *name == rx_name) else {
                    return;
                };
                let Some(src) = find(&|(name, serial)| *name == source || *serial == source)
                    .or_else(|| find(&|(name, _)| *name == instance_node))
                else {
                    return;
                };
                let port = |node, output| {
                    let mut ports: Vec<u32> = graph
                        .ports
                        .iter()
                        .filter(|&(_, &(n, out))| n == node && out == output)
                        .map(|(&id, _)| id)
                        .collect();
                    ports.sort();
                    ports
                };
                let Some(&input) = port(rx, false).first() else {
                    return;
                };
                for output in port(src, true) {
                    if graph.links.contains_key(&(output, input)) {
                        continue;
                    }
                    let props = properties! {
                        "link.output.node" => src.to_string(),
                        "link.output.port" => output.to_string(),
                        "link.input.node" => rx.to_string(),
                        "link.input.port" => input.to_string(),
                        "object.linger" => "false",
                    };
                    match core.create_object::<pw::link::Link>("link-factory", &props) {
                        Ok(link) => {
                            if !graph.links.keys().any(|&(_, i)| i == input) {
                                let name = &graph.nodes[&src].0;
                                events(Event::Info(format!("Linked {name} into {rx_name}")));
                            }
                            graph.links.insert((output, input), link);
                        }
                        Err(error) => events(Event::Error(format!("Could not link RX: {error}"))),
                    }
                }
            }
        };
        let update = Rc::new(update);
        let on_global = update.clone();
        let add_graph = graph.clone();
        let remove_graph = graph.clone();
        let listener = registry
            .add_listener_local()
            .global(move |global| {
                let Some(props) = global.props else {
                    return;
                };
                let get = |key| props.get(key).unwrap_or("").to_owned();
                match global.type_ {
                    pw::types::ObjectType::Node => {
                        add_graph
                            .borrow_mut()
                            .nodes
                            .insert(global.id, (get("node.name"), get("object.serial")));
                    }
                    pw::types::ObjectType::Port if get("port.monitor") != "true" => {
                        let Ok(node) = get("node.id").parse() else {
                            return;
                        };
                        let output = get("port.direction") == "out";
                        add_graph
                            .borrow_mut()
                            .ports
                            .insert(global.id, (node, output));
                    }
                    _ => return,
                }
                on_global();
            })
            .global_remove(move |id| {
                let mut graph = remove_graph.borrow_mut();
                graph.nodes.remove(&id);
                graph.ports.remove(&id);
                graph
                    .links
                    .retain(|&(output, input), _| output != id && input != id);
            })
            .register();
        Ok(Self {
            _registry: registry,
            _listener: listener,
        })
    }
}
