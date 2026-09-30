//! Push-to-talk around transmissions: key the radio, wait for it to switch,
//! send the queued audio, and unkey after a short tail.
//!
//! The controller runs on its own thread and never blocks on the radio: each key
//! or unkey runs on a short-lived worker thread and reports back. Rules, as in the
//! smc-bridge hrdctl plugin: an unknown keying outcome counts as keyed; a failed
//! unkey is retried until it succeeds; only a transmitter this controller keyed is
//! unkeyed; stopping, a failed key, the watchdog, and shutdown all unkey.

use crate::{
    audio::engine::{Command, CommandSender},
    cw::oscillator::SAMPLE_RATE,
};
use std::{
    collections::{HashSet, VecDeque},
    io::Read,
    path::PathBuf,
    process::{Command as Process, Stdio},
    sync::{
        Arc,
        mpsc::{self, RecvTimeoutError},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// Result of asking the radio to transmit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    Keyed,
    /// The request may or may not have reached the radio; treated as keyed.
    Unknown(String),
    Failed(String),
}

/// Something that can key and unkey a transmitter. Calls may block; the
/// controller runs them off its own thread.
pub trait Keyer: Send + Sync + 'static {
    fn key(&self) -> KeyOutcome;
    fn unkey(&self) -> Result<(), String>;
}

/// Keys through Ham Radio Deluxe with the `hrdctl` command (smc-bridge-hrdctl):
/// `hrdctl button <button> on` and `hrdctl unkey --button <button>`.
pub struct Hrdctl {
    program: PathBuf,
    options: Vec<String>,
    button: String,
    limit: Duration,
}

impl Hrdctl {
    pub fn new(
        program: impl Into<PathBuf>,
        host: Option<String>,
        port: Option<u16>,
        button: impl Into<String>,
    ) -> Self {
        let mut options = Vec::new();
        if let Some(host) = host {
            options.extend(["--host".to_owned(), host]);
        }
        if let Some(port) = port {
            options.extend(["--port".to_owned(), port.to_string()]);
        }
        Self {
            program: program.into(),
            options,
            button: button.into(),
            // hrdctl's own socket timeout is 5 s per operation; allow for several.
            limit: Duration::from_secs(20),
        }
    }

    /// Run hrdctl; the exit code (None if it had to be killed) and stderr.
    fn run(&self, args: &[&str]) -> Result<(Option<i32>, String), String> {
        let mut child = Process::new(&self.program)
            .args(&self.options)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not run {}: {e}", self.program.display()))?;
        let deadline = Instant::now() + self.limit;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        Ok((status.and_then(|s| s.code()), stderr.trim().to_owned()))
    }
}

impl Keyer for Hrdctl {
    fn key(&self) -> KeyOutcome {
        match self.run(&["button", &self.button, "on"]) {
            Ok((Some(0), _)) => KeyOutcome::Keyed,
            // hrdctl's exit code 3: the write's outcome is unknown.
            Ok((Some(3), message)) => KeyOutcome::Unknown(message),
            Ok((None, _)) => KeyOutcome::Unknown("hrdctl did not finish in time".into()),
            Ok((Some(code), message)) => {
                KeyOutcome::Failed(format!("hrdctl exited with {code}: {message}"))
            }
            Err(message) => KeyOutcome::Failed(message),
        }
    }

    fn unkey(&self) -> Result<(), String> {
        match self.run(&["unkey", "--button", &self.button])? {
            (Some(0), _) => Ok(()),
            (Some(code), message) => Err(format!("hrdctl exited with {code}: {message}")),
            (None, _) => Err("hrdctl did not finish in time".into()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PttConfig {
    /// Time from a confirmed key to the start of audio, for the radio to switch.
    pub lead: Duration,
    /// Time after the last audio before unkeying.
    pub tail: Duration,
    /// Unkey if still transmitting this long after the audio should have ended.
    pub watchdog_margin: Duration,
    /// Wait between unkey retries.
    pub retry: Duration,
}

impl Default for PttConfig {
    fn default() -> Self {
        Self {
            lead: Duration::from_millis(200),
            tail: Duration::from_millis(150),
            watchdog_margin: Duration::from_secs(10),
            retry: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PttEvent {
    /// A key request is on its way to the radio.
    Keying,
    /// The radio is (or may be) transmitting.
    Keyed,
    Unkeyed,
    /// A message was dropped before any of it was sent.
    MessageAborted(u64),
    Error(String),
}

enum Input {
    Submit { id: u64, samples: Vec<f32> },
    Abort,
    SetEnabled(bool),
    Finished(u64),
    KeyResult(KeyOutcome),
    UnkeyResult(Result<(), String>),
    Shutdown,
}

/// Cloneable, thread-safe link into the controller, for the engine's event
/// callback to report finished messages. Made before the controller starts, so it
/// can be handed to the engine that the controller then drives.
#[derive(Clone)]
pub struct PttLink(mpsc::Sender<Input>);

impl PttLink {
    /// The engine finished or aborted a message (Event::TxDone / TxAborted).
    pub fn finished(&self, id: u64) {
        let _ = self.0.send(Input::Finished(id));
    }
}

pub struct PttInbox {
    sender: mpsc::Sender<Input>,
    receiver: mpsc::Receiver<Input>,
}

pub fn link() -> (PttLink, PttInbox) {
    let (sender, receiver) = mpsc::channel();
    (PttLink(sender.clone()), PttInbox { sender, receiver })
}

/// Sends messages to the engine, keying the radio around them when enabled.
/// Dropping it stops everything and unkeys before returning.
pub struct Ptt {
    inbox: mpsc::Sender<Input>,
    thread: Option<JoinHandle<()>>,
}

impl Ptt {
    /// `keyer` is None for no radio control: messages go straight to the engine.
    pub fn start(
        inbox: PttInbox,
        keyer: Option<Arc<dyn Keyer>>,
        enabled: bool,
        config: PttConfig,
        engine: CommandSender,
        events: impl Fn(PttEvent) + Send + 'static,
    ) -> Self {
        Self::start_with(
            inbox,
            keyer,
            enabled,
            config,
            move |c| engine.send(c),
            events,
        )
    }

    fn start_with(
        inbox: PttInbox,
        keyer: Option<Arc<dyn Keyer>>,
        enabled: bool,
        config: PttConfig,
        engine: impl Fn(Command) + Send + 'static,
        events: impl Fn(PttEvent) + Send + 'static,
    ) -> Self {
        let sender = inbox.sender.clone();
        let thread = std::thread::Builder::new()
            .name("ptt".into())
            .spawn(move || {
                Controller {
                    enabled: enabled && keyer.is_some(),
                    keyer,
                    config,
                    engine: Box::new(engine),
                    events: Box::new(events),
                    workers: inbox.sender,
                    pending: VecDeque::new(),
                    outstanding: HashSet::new(),
                    keyed: false,
                    busy: false,
                    leading: None,
                    tail: None,
                    retry: None,
                    watchdog: None,
                    unkey_wanted: false,
                    shutting_down: false,
                }
                .run(inbox.receiver)
            })
            .expect("spawn ptt thread");
        Self {
            inbox: sender,
            thread: Some(thread),
        }
    }

    pub fn submit(&self, id: u64, samples: Vec<f32>) {
        let _ = self.inbox.send(Input::Submit { id, samples });
    }

    /// Stop sending, drop queued messages, and unkey.
    pub fn abort(&self) {
        let _ = self.inbox.send(Input::Abort);
    }

    /// Turn keying on or off; turning it off while transmitting unkeys first.
    pub fn set_enabled(&self, enabled: bool) {
        let _ = self.inbox.send(Input::SetEnabled(enabled));
    }
}

impl Drop for Ptt {
    fn drop(&mut self) {
        let _ = self.inbox.send(Input::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Controller {
    keyer: Option<Arc<dyn Keyer>>,
    enabled: bool,
    config: PttConfig,
    engine: Box<dyn Fn(Command) + Send>,
    events: Box<dyn Fn(PttEvent) + Send>,
    /// For worker threads to report results.
    workers: mpsc::Sender<Input>,
    /// Waiting for the radio to key (not yet given to the engine).
    pending: VecDeque<(u64, Vec<f32>)>,
    /// Given to the engine and not yet reported finished.
    outstanding: HashSet<u64>,
    /// We keyed the transmitter (or may have) and have not unkeyed it.
    keyed: bool,
    /// A key or unkey is in progress on a worker thread.
    busy: bool,
    leading: Option<Instant>,
    tail: Option<Instant>,
    retry: Option<Instant>,
    watchdog: Option<Instant>,
    unkey_wanted: bool,
    shutting_down: bool,
}

/// Give up retrying a failed unkey at shutdown after this long.
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(30);

impl Controller {
    fn run(mut self, inbox: mpsc::Receiver<Input>) {
        let mut shutdown_deadline = None;
        loop {
            if self.shutting_down && !self.keyed && !self.busy {
                return;
            }
            if shutdown_deadline.is_some_and(|d| Instant::now() >= d) {
                (self.events)(PttEvent::Error(
                    "Could not unkey the radio before closing; check it now".into(),
                ));
                return;
            }
            let next = [self.leading, self.tail, self.retry, self.watchdog]
                .into_iter()
                .flatten()
                .chain(shutdown_deadline)
                .min();
            let input = match next {
                Some(at) => {
                    match inbox.recv_timeout(at.saturating_duration_since(Instant::now())) {
                        Ok(input) => Some(input),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => Some(Input::Shutdown),
                    }
                }
                None => Some(inbox.recv().unwrap_or(Input::Shutdown)),
            };
            match input {
                Some(Input::Shutdown) => {
                    if !self.shutting_down {
                        self.shutting_down = true;
                        shutdown_deadline = Some(Instant::now() + SHUTDOWN_LIMIT);
                        self.abort();
                    }
                }
                Some(input) => self.handle(input),
                None => self.timers(),
            }
            self.advance();
        }
    }

    fn handle(&mut self, input: Input) {
        match input {
            Input::Submit { id, samples } if self.shutting_down => {
                drop(samples);
                (self.events)(PttEvent::MessageAborted(id));
            }
            Input::Submit { id, samples } => {
                if !self.enabled && !self.keyed && !self.busy {
                    // No radio control: straight to the engine (VOX or manual PTT).
                    // Never while a transmitter we keyed may still be keyed or a key
                    // or unkey is in flight: those wait in the queue (see advance).
                    (self.engine)(Command::Send { id, samples });
                } else if self.enabled
                    && self.keyed
                    && !self.busy
                    && self.leading.is_none()
                    && !self.unkey_wanted
                {
                    // Already on the air: keep the transmitter keyed and send.
                    self.tail = None;
                    self.send(id, samples);
                } else {
                    self.pending.push_back((id, samples));
                }
            }
            Input::Abort => self.abort(),
            Input::SetEnabled(enabled) => {
                let enabled = enabled && self.keyer.is_some();
                let active = self.keyed
                    || self.busy
                    || !self.pending.is_empty()
                    || !self.outstanding.is_empty();
                if self.enabled && !enabled && active {
                    self.abort();
                }
                self.enabled = enabled;
            }
            Input::Finished(id) => {
                if self.outstanding.remove(&id)
                    && self.outstanding.is_empty()
                    && self.pending.is_empty()
                    && self.keyed
                {
                    self.tail = Some(Instant::now() + self.config.tail);
                }
            }
            Input::KeyResult(outcome) => {
                self.busy = false;
                match outcome {
                    KeyOutcome::Keyed => {
                        self.keyed = true;
                        (self.events)(PttEvent::Keyed);
                    }
                    KeyOutcome::Unknown(message) => {
                        self.keyed = true;
                        (self.events)(PttEvent::Keyed);
                        (self.events)(PttEvent::Error(format!(
                            "Keying outcome unknown, treating the radio as keyed{}",
                            if message.is_empty() {
                                String::new()
                            } else {
                                format!(": {message}")
                            }
                        )));
                    }
                    KeyOutcome::Failed(message) => {
                        (self.events)(PttEvent::Error(format!(
                            "Could not key the radio: {message}"
                        )));
                        // Keying was switched off meanwhile: the radio is not keyed,
                        // so queued messages go out as plain audio (see advance).
                        if self.enabled {
                            self.drop_pending();
                        }
                        return;
                    }
                }
                if self.unkey_wanted
                    || self.pending.is_empty()
                    || self.shutting_down
                    || !self.enabled
                {
                    self.unkey_wanted = true;
                } else {
                    self.leading = Some(Instant::now() + self.config.lead);
                    self.arm_watchdog();
                }
            }
            Input::UnkeyResult(result) => {
                self.busy = false;
                match result {
                    Ok(()) => {
                        self.keyed = false;
                        self.unkey_wanted = false;
                        self.watchdog = None;
                        (self.events)(PttEvent::Unkeyed);
                    }
                    Err(message) => {
                        (self.events)(PttEvent::Error(format!(
                            "Could not unkey the radio, retrying: {message}"
                        )));
                        self.retry = Some(Instant::now() + self.config.retry);
                    }
                }
            }
            Input::Shutdown => unreachable!(),
        }
    }

    fn timers(&mut self) {
        let now = Instant::now();
        if self.leading.is_some_and(|t| now >= t) {
            self.leading = None;
            for (id, samples) in std::mem::take(&mut self.pending) {
                self.send(id, samples);
            }
        }
        if self.tail.is_some_and(|t| now >= t) {
            self.tail = None;
            self.unkey_wanted = true;
        }
        if self.retry.is_some_and(|t| now >= t) {
            self.retry = None;
        }
        if self.watchdog.is_some_and(|t| now >= t) {
            self.watchdog = None;
            (self.events)(PttEvent::Error(
                "Transmit watchdog: still keyed after the audio should have ended; unkeying".into(),
            ));
            self.abort();
        }
    }

    /// Start whatever the state calls for next, if the radio is not busy.
    fn advance(&mut self) {
        if self.busy || self.retry.is_some() {
            return;
        }
        if !self.enabled && !self.keyed {
            // Keying is off and the transmitter we keyed is confirmed unkeyed:
            // messages queued meanwhile go straight to the engine.
            for (id, samples) in std::mem::take(&mut self.pending) {
                (self.engine)(Command::Send { id, samples });
            }
            return;
        }
        let Some(keyer) = self.keyer.clone() else {
            return;
        };
        if self.keyed && self.unkey_wanted {
            self.busy = true;
            let workers = self.workers.clone();
            std::thread::spawn(move || {
                let _ = workers.send(Input::UnkeyResult(keyer.unkey()));
            });
        } else if !self.keyed && self.enabled && !self.pending.is_empty() && !self.shutting_down {
            self.unkey_wanted = false;
            self.busy = true;
            (self.events)(PttEvent::Keying);
            let workers = self.workers.clone();
            std::thread::spawn(move || {
                let _ = workers.send(Input::KeyResult(keyer.key()));
            });
        }
    }

    fn send(&mut self, id: u64, samples: Vec<f32>) {
        self.outstanding.insert(id);
        self.extend_watchdog(samples.len());
        (self.engine)(Command::Send { id, samples });
    }

    /// Stop the engine, drop queued messages, and unkey if we keyed (or are keying).
    fn abort(&mut self) {
        (self.engine)(Command::Abort);
        self.outstanding.clear();
        self.drop_pending();
        self.leading = None;
        self.tail = None;
        if self.keyed || self.busy {
            self.unkey_wanted = true;
        }
    }

    fn drop_pending(&mut self) {
        for (id, _) in self.pending.drain(..) {
            (self.events)(PttEvent::MessageAborted(id));
        }
    }

    fn arm_watchdog(&mut self) {
        let audio: usize = self.pending.iter().map(|(_, s)| s.len()).sum();
        self.watchdog = Some(
            Instant::now()
                + self.config.lead
                + Self::duration(audio)
                + self.config.tail
                + self.config.watchdog_margin,
        );
    }

    fn extend_watchdog(&mut self, samples: usize) {
        // Pending audio was counted when the watchdog was armed.
        if self.leading.is_none()
            && let Some(watchdog) = &mut self.watchdog
        {
            let earliest = Instant::now()
                + Self::duration(samples)
                + self.config.tail
                + self.config.watchdog_margin;
            *watchdog = (*watchdog + Self::duration(samples)).max(earliest);
        }
    }

    fn duration(samples: usize) -> Duration {
        Duration::from_secs_f64(samples as f64 / SAMPLE_RATE as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what happened, in order: "key", "unkey", "send <id>", "abort",
    /// and events.
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<String>>>);
    impl Log {
        fn push(&self, entry: impl Into<String>) {
            self.0.lock().unwrap().push(entry.into());
        }
        fn entries(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
        fn actions(&self) -> Vec<String> {
            self.entries()
                .into_iter()
                .filter(|e| !e.starts_with("event"))
                .collect()
        }
        fn wait_count(&self, entry: &str, count: usize) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.entries().iter().filter(|e| *e == entry).count() < count {
                assert!(
                    Instant::now() < deadline,
                    "{entry:?} x{count} in {:?}",
                    self.entries()
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        fn wait_for(&self, entry: &str) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self.entries().iter().any(|e| e == entry) {
                assert!(
                    Instant::now() < deadline,
                    "no {entry:?} in {:?}",
                    self.entries()
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    struct FakeKeyer {
        log: Log,
        delay: Duration,
        key: Mutex<Vec<KeyOutcome>>,
        unkey_failures: Mutex<u32>,
    }
    impl Keyer for FakeKeyer {
        fn key(&self) -> KeyOutcome {
            std::thread::sleep(self.delay);
            self.log.push("key");
            self.key.lock().unwrap().pop().unwrap_or(KeyOutcome::Keyed)
        }
        fn unkey(&self) -> Result<(), String> {
            std::thread::sleep(self.delay);
            let mut failures = self.unkey_failures.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                self.log.push("unkey failed");
                return Err("HRD unreachable".into());
            }
            self.log.push("unkey");
            Ok(())
        }
    }

    fn config() -> PttConfig {
        PttConfig {
            lead: Duration::from_millis(20),
            tail: Duration::from_millis(20),
            watchdog_margin: Duration::from_millis(200),
            retry: Duration::from_millis(20),
        }
    }

    fn start(
        key: Vec<KeyOutcome>,
        unkey_failures: u32,
        delay: Duration,
        config: PttConfig,
    ) -> (Ptt, PttLink, Log) {
        let log = Log::default();
        let keyer = Arc::new(FakeKeyer {
            log: log.clone(),
            delay,
            key: Mutex::new(key),
            unkey_failures: Mutex::new(unkey_failures),
        });
        let (link, inbox) = link();
        let (engine_log, event_log) = (log.clone(), log.clone());
        let ptt = Ptt::start_with(
            inbox,
            Some(keyer),
            true,
            config,
            move |command| match command {
                Command::Send { id, .. } => engine_log.push(format!("send {id}")),
                Command::Abort => engine_log.push("abort"),
                _ => {}
            },
            move |event| event_log.push(format!("event {event:?}")),
        );
        (ptt, link, log)
    }

    /// One second of audio.
    fn audio() -> Vec<f32> {
        vec![0.0; SAMPLE_RATE as usize]
    }

    #[test]
    fn keys_sends_and_unkeys_after_the_tail() {
        let (ptt, link, log) = start(vec![], 0, Duration::ZERO, config());
        ptt.submit(1, audio());
        log.wait_for("send 1");
        link.finished(1);
        log.wait_for("unkey");
        assert_eq!(log.actions(), ["key", "send 1", "unkey"]);
        assert!(log.entries().contains(&"event Keyed".to_owned()));
        assert!(log.entries().contains(&"event Unkeyed".to_owned()));
    }

    #[test]
    fn back_to_back_messages_stay_keyed() {
        let (ptt, link, log) = start(vec![], 0, Duration::ZERO, config());
        ptt.submit(1, audio());
        ptt.submit(2, audio());
        log.wait_for("send 2");
        link.finished(1);
        // Sent while still on the air: no second key.
        ptt.submit(3, audio());
        log.wait_for("send 3");
        link.finished(2);
        link.finished(3);
        log.wait_for("unkey");
        // After unkeying, the next message keys again.
        ptt.submit(4, audio());
        log.wait_for("send 4");
        link.finished(4);
        log.wait_count("unkey", 2);
        assert_eq!(
            log.actions(),
            [
                "key", "send 1", "send 2", "send 3", "unkey", "key", "send 4", "unkey"
            ]
        );
    }

    #[test]
    fn message_during_the_tail_is_sent_without_unkeying() {
        let mut slow_tail = config();
        slow_tail.tail = Duration::from_millis(300);
        let (ptt, link, log) = start(vec![], 0, Duration::ZERO, slow_tail);
        ptt.submit(1, audio());
        log.wait_for("send 1");
        link.finished(1);
        ptt.submit(2, audio());
        log.wait_for("send 2");
        link.finished(2);
        log.wait_for("unkey");
        assert_eq!(log.actions(), ["key", "send 1", "send 2", "unkey"]);
    }

    #[test]
    fn stop_while_sending_unkeys() {
        let (ptt, _link, log) = start(vec![], 0, Duration::ZERO, config());
        ptt.submit(1, audio());
        log.wait_for("send 1");
        ptt.abort();
        log.wait_for("unkey");
        assert_eq!(log.actions(), ["key", "send 1", "abort", "unkey"]);
    }

    #[test]
    fn stop_while_keying_drops_the_message_and_unkeys() {
        let (ptt, _link, log) = start(vec![], 0, Duration::from_millis(50), config());
        ptt.submit(1, audio());
        std::thread::sleep(Duration::from_millis(10));
        ptt.abort();
        log.wait_for("unkey");
        assert_eq!(log.actions(), ["abort", "key", "unkey"]);
        assert!(
            log.entries()
                .contains(&"event MessageAborted(1)".to_owned())
        );
    }

    #[test]
    fn failed_key_sends_nothing() {
        let failed = KeyOutcome::Failed("connection refused".into());
        let (ptt, _link, log) = start(vec![failed], 0, Duration::ZERO, config());
        ptt.submit(1, audio());
        log.wait_for("event MessageAborted(1)");
        drop(ptt);
        assert_eq!(log.actions(), ["key", "abort"]);
    }

    #[test]
    fn unknown_key_outcome_is_treated_as_keyed() {
        let unknown = KeyOutcome::Unknown("timed out".into());
        let (ptt, link, log) = start(vec![unknown], 0, Duration::ZERO, config());
        ptt.submit(1, audio());
        log.wait_for("send 1");
        link.finished(1);
        log.wait_for("unkey");
        assert_eq!(log.actions(), ["key", "send 1", "unkey"]);
    }

    #[test]
    fn failed_unkey_is_retried() {
        let (ptt, link, log) = start(vec![], 2, Duration::ZERO, config());
        ptt.submit(1, audio());
        log.wait_for("send 1");
        link.finished(1);
        log.wait_for("unkey");
        assert_eq!(
            log.actions(),
            ["key", "send 1", "unkey failed", "unkey failed", "unkey"]
        );
    }

    #[test]
    fn closing_while_keyed_unkeys_first() {
        let (ptt, _link, log) = start(vec![], 0, Duration::from_millis(20), config());
        ptt.submit(1, audio());
        log.wait_for("send 1");
        drop(ptt);
        assert_eq!(log.actions(), ["key", "send 1", "abort", "unkey"]);
    }

    #[test]
    fn watchdog_unkeys_when_audio_never_finishes() {
        let (ptt, _link, log) = start(vec![], 0, Duration::ZERO, config());
        // Short audio, and the engine never reports it finished.
        ptt.submit(1, vec![0.0; 480]);
        log.wait_for("unkey");
        assert_eq!(log.actions(), ["key", "send 1", "abort", "unkey"]);
        assert!(
            log.entries()
                .iter()
                .any(|e| e.contains("Transmit watchdog"))
        );
        drop(ptt);
    }

    #[test]
    fn disabled_sends_straight_to_the_engine() {
        let (ptt, _link, log) = start(vec![], 0, Duration::ZERO, config());
        ptt.set_enabled(false);
        ptt.submit(1, audio());
        log.wait_for("send 1");
        drop(ptt);
        assert_eq!(log.actions(), ["send 1", "abort"]);
    }

    #[test]
    fn disabling_while_keyed_holds_messages_until_unkeyed() {
        // A slow unkey: the next message must not reach the engine before it ends.
        let (ptt, _link, log) = start(vec![], 0, Duration::from_millis(100), config());
        ptt.submit(1, audio());
        log.wait_for("send 1");
        ptt.set_enabled(false);
        ptt.submit(2, audio());
        log.wait_for("send 2");
        drop(ptt);
        let actions = log.actions();
        let position = |a: &str| actions.iter().position(|x| x == a).unwrap();
        assert!(position("unkey") < position("send 2"), "{actions:?}");
        assert_eq!(&actions[..4], ["key", "send 1", "abort", "unkey"]);
    }

    #[test]
    fn disabling_with_a_failing_unkey_holds_messages_until_it_succeeds() {
        let (ptt, _link, log) = start(vec![], 2, Duration::ZERO, config());
        ptt.submit(1, audio());
        log.wait_for("send 1");
        ptt.set_enabled(false);
        ptt.submit(2, audio());
        log.wait_for("send 2");
        drop(ptt);
        assert_eq!(
            &log.actions()[..6],
            [
                "key",
                "send 1",
                "abort",
                "unkey failed",
                "unkey failed",
                "unkey"
            ]
        );
        assert_eq!(log.actions()[6], "send 2");
    }

    #[test]
    fn disabling_while_keying_unkeys_then_sends_as_audio() {
        let (ptt, _link, log) = start(vec![], 0, Duration::from_millis(50), config());
        ptt.submit(1, audio());
        std::thread::sleep(Duration::from_millis(10));
        ptt.set_enabled(false);
        ptt.submit(2, audio());
        log.wait_for("send 2");
        drop(ptt);
        let actions = log.actions();
        // Message 1 was dropped by the abort; 2 waited for the key to be undone.
        assert_eq!(
            &actions[..4],
            ["abort", "key", "unkey", "send 2"],
            "{actions:?}"
        );
        assert!(!actions.contains(&"send 1".to_owned()));
    }

    #[test]
    fn hrdctl_commands_and_exit_codes() {
        let dir = std::env::temp_dir().join(format!("cw-chat-hrdctl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("hrdctl");
        let calls = dir.join("calls");
        let code = dir.join("code");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" >> {}\necho oops >&2\nexit $(cat {})\n",
                calls.display(),
                code.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hrdctl = Hrdctl::new(&script, Some("10.0.0.5".into()), Some(7809), "MOX");
        let with_code = |c: &str| std::fs::write(&code, c).unwrap();

        with_code("0");
        assert_eq!(hrdctl.key(), KeyOutcome::Keyed);
        assert_eq!(hrdctl.unkey(), Ok(()));
        with_code("3");
        assert_eq!(hrdctl.key(), KeyOutcome::Unknown("oops".into()));
        with_code("1");
        assert!(matches!(hrdctl.key(), KeyOutcome::Failed(m) if m.contains("oops")));
        assert!(hrdctl.unkey().is_err());
        let missing = Hrdctl::new(dir.join("missing"), None, None, "TX");
        assert!(matches!(missing.key(), KeyOutcome::Failed(_)));

        let calls = std::fs::read_to_string(&calls).unwrap();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            [
                "--host 10.0.0.5 --port 7809 button MOX on",
                "--host 10.0.0.5 --port 7809 unkey --button MOX",
                "--host 10.0.0.5 --port 7809 button MOX on",
                "--host 10.0.0.5 --port 7809 button MOX on",
                "--host 10.0.0.5 --port 7809 unkey --button MOX",
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
