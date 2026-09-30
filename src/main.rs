mod ui;

use clap::{Args, Parser, Subcommand, ValueEnum};
use cw_chat::{
    audio::{
        self,
        engine::{self, Engine, Event},
    },
    cw::{oscillator, timing},
    morse::encoder,
    ptt::{self, Hrdctl, Keyer, Ptt, PttConfig, PttEvent},
    rx::decoder::{self, RxEvent},
};
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
    sync::{Arc, mpsc},
    time::Duration,
};

#[derive(Parser)]
#[command(
    version,
    about = "CW chat over native PipeWire: opens the chat window unless a subcommand is given",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Mode>,
    #[command(flatten)]
    engine: EngineArgs,
    #[command(flatten)]
    tone: ToneArgs,
    #[command(flatten)]
    ptt: PttArgs,
}

#[derive(Subcommand)]
enum Mode {
    /// Send one message and exit (or export it as WAV)
    Send(SendArgs),
    /// Decode CW from a WAV file
    Decode(DecodeArgs),
    /// Terminal chat: lines from stdin are sent, decoded RX is printed
    Console {
        #[command(flatten)]
        engine: EngineArgs,
        #[command(flatten)]
        tone: ToneArgs,
        #[command(flatten)]
        ptt: PttArgs,
    },
}

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq)]
pub enum PttBackend {
    /// Ham Radio Deluxe, through the hrdctl command (smc-bridge-hrdctl)
    Hrdctl,
}

#[derive(Args, Clone)]
pub struct PttArgs {
    /// Key the radio around each message (put the radio in DATA-U/USB)
    #[arg(long, value_enum)]
    pub ptt: Option<PttBackend>,
    /// hrdctl command to run
    #[arg(long, default_value = "hrdctl")]
    pub hrdctl: PathBuf,
    /// HRD host [default: hrdctl's, or HRD_HOST]
    #[arg(long)]
    pub hrd_host: Option<String>,
    /// HRD port [default: hrdctl's, or HRD_PORT]
    #[arg(long)]
    pub hrd_port: Option<u16>,
    /// HRD button that keys the transmitter
    #[arg(long, default_value = "TX")]
    pub ptt_button: String,
    /// Milliseconds from keying to the start of audio
    #[arg(long, default_value_t = 200)]
    pub ptt_lead_ms: u64,
    /// Milliseconds after the audio before unkeying
    #[arg(long, default_value_t = 150)]
    pub ptt_tail_ms: u64,
}

impl PttArgs {
    pub fn keyer(&self) -> Option<Arc<dyn Keyer>> {
        match self.ptt? {
            PttBackend::Hrdctl => Some(Arc::new(Hrdctl::new(
                &self.hrdctl,
                self.hrd_host.clone(),
                self.hrd_port,
                &self.ptt_button,
            ))),
        }
    }

    pub fn config(&self) -> PttConfig {
        PttConfig {
            lead: Duration::from_millis(self.ptt_lead_ms),
            tail: Duration::from_millis(self.ptt_tail_ms),
            ..PttConfig::default()
        }
    }
}

#[derive(Args, Clone)]
pub struct ToneArgs {
    /// TX speed
    #[arg(long, default_value_t = 20.0)]
    pub wpm: f64,
    /// TX tone in Hz; RX listens here too unless --rx-tone is given
    #[arg(long, default_value_t = 700.0)]
    pub tone: f64,
    #[arg(long, default_value_t = 0.2)]
    pub gain: f64,
}

#[derive(Args, Clone)]
pub struct EngineArgs {
    /// Instance name, for running several copies (nodes: cw-chat-NAME-tx/-rx)
    #[arg(long)]
    pub name: Option<String>,
    /// PipeWire target for TX: node.name or object.serial
    #[arg(long)]
    pub target: Option<String>,
    /// PipeWire target for RX, routed by the session manager
    #[arg(long, conflicts_with = "rx_from")]
    pub rx_target: Option<String>,
    /// Link this node (or another instance's name) straight into RX
    #[arg(long)]
    pub rx_from: Option<String>,
    /// Leave both nodes unconnected, for routing in qpwgraph
    #[arg(long)]
    pub manual: bool,
    /// Fix the RX tone in Hz; without it, RX follows the signal automatically,
    /// starting from the TX tone
    #[arg(long)]
    pub rx_tone: Option<f64>,
    /// Keep decoding while transmitting
    #[arg(long)]
    pub full_duplex: bool,
}

impl EngineArgs {
    pub fn config(&self, tone: &ToneArgs) -> engine::Config {
        engine::Config {
            name: self.name.clone(),
            tx_target: self.target.clone(),
            rx_target: self.rx_target.clone(),
            rx_from: self.rx_from.clone(),
            manual: self.manual,
            rx_tone: self.rx_tone.unwrap_or(tone.tone),
            rx_auto: self.rx_tone.is_none(),
            rx_wpm_hint: tone.wpm,
            rx_mute: !self.full_duplex,
        }
    }
}

#[derive(Args)]
struct SendArgs {
    /// Text to transmit; when omitted, prompt for one line
    text: Vec<String>,
    #[command(flatten)]
    tone: ToneArgs,
    /// PipeWire target node.name or object.serial
    #[arg(long, conflicts_with = "manual")]
    target: Option<String>,
    /// Wait for a manual link in qpwgraph (message duration + 120 seconds)
    #[arg(long)]
    manual: bool,
    /// Export a mono 48 kHz float WAV instead of playing
    #[arg(long)]
    wav: Option<PathBuf>,
}

#[derive(Args)]
struct DecodeArgs {
    file: PathBuf,
    /// Tone in Hz; without it, the tone is found automatically
    #[arg(long)]
    tone: Option<f64>,
    /// Starting speed estimate; the decoder adapts to the sender
    #[arg(long, default_value_t = 20.0)]
    wpm: f64,
}

type Error = Box<dyn std::error::Error>;

/// Encode and render one message, followed by a word gap so queued messages stay apart.
pub fn render(text: &str, tone: &ToneArgs) -> Result<(encoder::Message, Vec<f32>), Error> {
    if text.len() > 4096 {
        return Err("Text is limited to 4096 bytes per transmission".into());
    }
    let message = encoder::encode(text)?;
    let mut samples =
        oscillator::render(&timing::schedule(&message), tone.wpm, tone.tone, tone.gain)?;
    samples.extend(oscillator::gap(7, tone.wpm));
    Ok((message, samples))
}

fn send(args: SendArgs) -> Result<(), Error> {
    let text = if args.text.is_empty() {
        print!("TX> ");
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        line
    } else {
        args.text.join(" ")
    };
    if text.len() > 4096 {
        return Err("Text is limited to 4096 bytes per transmission".into());
    }
    let tone = &args.tone;
    let message = encoder::encode(&text)?;
    let samples = oscillator::render(&timing::schedule(&message), tone.wpm, tone.tone, tone.gain)?;
    println!("{message}");
    println!(
        "{:.2}s at {} WPM, {} Hz",
        samples.len() as f64 / oscillator::SAMPLE_RATE as f64,
        tone.wpm,
        tone.tone
    );
    if let Some(path) = args.wav {
        let mut writer = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 1,
                sample_rate: oscillator::SAMPLE_RATE,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )?;
        for sample in samples {
            writer.write_sample(sample)?;
        }
        writer.finalize()?;
        println!("Wrote {}", path.display());
    } else {
        println!("PipeWire node: morse-tx");
        if args.manual {
            println!("Connect its output in qpwgraph to begin.");
        }
        audio::pipewire::play(samples, args.target.as_deref(), args.manual)?;
    }
    Ok(())
}

fn decode(args: DecodeArgs) -> Result<(), Error> {
    let mut reader = hound::WavReader::open(&args.file)?;
    let spec = reader.spec();
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|s| s as f32 / scale))
                .collect::<Result<_, _>>()?
        }
    };
    // Mix all channels down to mono.
    let channels = spec.channels.max(1) as usize;
    let mono: Vec<f32> = raw
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect();
    println!(
        "{}",
        decoder::decode_all_with(
            &mono,
            spec.sample_rate,
            args.tone.unwrap_or(700.0),
            args.wpm,
            args.tone.is_none()
        )
    );
    Ok(())
}

enum Input {
    Line(String),
    Eof,
    Engine(Event),
    Ptt(PttEvent),
}

/// Terminal chat on the engine; exits after stdin closes and queued messages finish.
fn console(engine_args: EngineArgs, tone: ToneArgs, ptt_args: PttArgs) -> Result<(), Error> {
    let (input, inbox) = mpsc::channel();
    let (engine_input, ptt_input) = (input.clone(), input.clone());
    let (ptt_link, ptt_inbox) = ptt::link();
    let engine = Engine::start(engine_args.config(&tone), move |event| {
        if let Event::TxDone(id) | Event::TxAborted(id) = event {
            ptt_link.finished(id);
        }
        let _ = engine_input.send(Input::Engine(event));
    })?;
    // Declared after the engine, so it is dropped (and unkeys) first.
    let ptt = Ptt::start(
        ptt_inbox,
        ptt_args.keyer(),
        ptt_args.ptt.is_some(),
        ptt_args.config(),
        engine.sender(),
        move |event| {
            let _ = ptt_input.send(Input::Ptt(event));
        },
    );
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines().map_while(Result::ok) {
            let _ = input.send(Input::Line(line));
        }
        let _ = input.send(Input::Eof);
    });
    let (mut next_id, mut pending, mut closing, mut rx_line) = (0u64, 0usize, false, false);
    let mut out = io::stdout();
    loop {
        match inbox.recv()? {
            Input::Line(line) if line.trim().is_empty() => {}
            Input::Line(line) => match render(&line, &tone) {
                Ok((message, samples)) => {
                    next_id += 1;
                    pending += 1;
                    if rx_line {
                        writeln!(out)?;
                        rx_line = false;
                    }
                    writeln!(
                        out,
                        "TX #{next_id}: {} ({message})",
                        line.trim().to_uppercase()
                    )?;
                    ptt.submit(next_id, samples);
                }
                Err(error) => eprintln!("cw-chat: {error}"),
            },
            Input::Eof => closing = true,
            Input::Engine(Event::TxDone(id) | Event::TxAborted(id))
            | Input::Ptt(PttEvent::MessageAborted(id)) => {
                pending -= 1;
                writeln!(out, "TX #{id} done")?;
            }
            Input::Ptt(PttEvent::Keying) => eprintln!("cw-chat: keying the radio"),
            Input::Ptt(PttEvent::Keyed) => eprintln!("cw-chat: ON AIR"),
            Input::Ptt(PttEvent::Unkeyed) => eprintln!("cw-chat: unkeyed"),
            Input::Ptt(PttEvent::Error(message)) => eprintln!("cw-chat: {message}"),
            // An over that ended with nothing printed since (e.g. our TX interrupted it).
            Input::Engine(Event::Rx(RxEvent::Idle)) if !rx_line => {}
            Input::Engine(Event::Rx(RxEvent::OverUpdate(_))) => {}
            Input::Engine(Event::Rx(RxEvent::Over(over))) => {
                if rx_line {
                    writeln!(out)?;
                }
                write!(
                    out,
                    "RX station {} ({:.0} Hz, {:.0} dBFS): ",
                    (b'A' + (over.station % 26) as u8) as char,
                    over.pitch_hz,
                    over.level_db
                )?;
                rx_line = true;
            }
            Input::Engine(Event::Rx(event)) => {
                if !rx_line {
                    write!(out, "RX: ")?;
                    rx_line = true;
                }
                match event {
                    RxEvent::Char(c) => write!(out, "{c}")?,
                    RxEvent::WordGap => write!(out, " ")?,
                    RxEvent::Idle => {
                        writeln!(out)?;
                        rx_line = false;
                    }
                    RxEvent::Over(_) | RxEvent::OverUpdate(_) => unreachable!(),
                }
                out.flush()?;
            }
            Input::Engine(Event::Info(message)) => eprintln!("cw-chat: {message}"),
            Input::Engine(Event::Error(message)) => eprintln!("cw-chat: {message}"),
            Input::Engine(Event::Stopped) => return Err("PipeWire engine stopped".into()),
            Input::Engine(_) => {}
        }
        if closing && pending == 0 {
            if rx_line {
                writeln!(out)?;
            }
            return Ok(());
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Mode::Send(args)) => send(args),
        Some(Mode::Decode(args)) => decode(args),
        Some(Mode::Console { engine, tone, ptt }) => console(engine, tone, ptt),
        None => ui::run(cli.engine, cli.tone, cli.ptt),
    };
    if let Err(error) = result {
        eprintln!("cw-chat: {error}");
        std::process::exit(1);
    }
}
