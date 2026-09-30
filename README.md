# cw-chat

A CW (Morse) chat app for Linux: a GTK4/libadwaita window that sends typed text as
CW audio and decodes received CW into the same transcript, over native PipeWire.
No CAT, PTT, or radio control yet.

## Install

From the AUR, as `cw-chat`, with an AUR helper or by hand:

```sh
git clone https://aur.archlinux.org/cw-chat.git
cd cw-chat
makepkg -si
```

It installs `/usr/bin/cw-chat` and a "CW Chat" launcher entry. The package builds a
tagged release from GitHub and runs the test suite first.

The launcher icon is a white Morse key on a teal-blue background. The source image is
`assets/icons/net.cwchat.CwChat.png`, with 16–512 px sizes beside it; the package
installs those to `/usr/share/icons/hicolor/<size>/apps/net.cwchat.CwChat.png`, and
the desktop entry refers to it as `Icon=net.cwchat.CwChat`.

## Build

Requires Linux, Rust/Cargo, pkg-config, Clang/libclang, PipeWire development
headers, and GTK 4.12+ with libadwaita 1.5+. On Debian/Ubuntu the native build
packages are `build-essential pkg-config clang libclang-dev libpipewire-0.3-dev
libgtk-4-dev libadwaita-1-dev`; on Arch, `base-devel clang pipewire gtk4 libadwaita`.

```sh
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

## Chat window

```sh
cargo run --release
```

The window shows sent overs on the right (text, Morse, send progress) and received
overs on the left, filled in character by character as they are decoded. A new RX
bubble starts after 2.5 seconds of silence. Below the transcript are the RX level
meter, a key indicator lit while a tone is detected, and the sender's estimated
speed. Press Enter or Send to transmit; messages sent while another is playing are
queued. Stop (or Esc) aborts the current message and clears the queue. The settings
menu adjusts TX speed, tone, and gain, and whether RX is muted while sending (on by
default, so your own audio is not decoded).

The **Tone** slider under the RX meter sets the audio pitch the decoder listens for
(300–1200 Hz). It must match the pitch your radio produces, within about 50 Hz; a
mismatch shows up as strings of `E`. With **Auto** on (the default), the decoder
finds the strongest CW signal in that range, follows it as it drifts, and moves to
each station as it starts sending; the slider shows where it has tuned. Dragging
the slider switches to a fixed tone; click Auto to hand control back.

Received overs are split and labelled by station: after a pause, a character more
than 15 Hz or 10 dB away from the current over starts a new one, and stations are
remembered (Station A, B, ...) with their pitch and level in the bubble header.
Right-click the transcript for Copy Message, Copy All (Ctrl+Shift+C), New Line
(Ctrl+Enter, ends the current RX bubble now), and Clear (Ctrl+L); all bubble text
is selectable.

Each copy creates two PipeWire nodes: `cw-chat-tx` (playback) and `cw-chat-rx`
(capture). By default TX goes to the default sink and RX listens to the default
source, both routed by the session manager. Options (also accepted by `console`):

```text
--name NAME         instance name; nodes become cw-chat-NAME-tx / cw-chat-NAME-rx
--target NODE       TX target: node.name or object.serial
--rx-target NODE    RX source, routed by the session manager
--rx-from NODE      link NODE (or another instance's NAME) straight into RX
--manual            leave both nodes unconnected, for routing in qpwgraph
--wpm, --tone, --gain   TX settings (defaults 20 WPM, 700 Hz, 0.2)
--rx-tone HZ        fix the RX tone (default: automatic, starting at the TX tone)
--full-duplex       keep decoding while transmitting
```

Inspect nodes with `pw-cli ls Node`. For a radio, route its receive audio (a USB
sound card or SDR output) to RX with `--rx-target` or qpwgraph.

### Two copies talking to each other

Each launch is an independent instance, so two copies can hold a QSO locally:

```sh
cw-chat --name A --rx-from B &
cw-chat --name B --rx-from A
```

`--rx-from` creates the links itself (via PipeWire's link factory), in whichever
order the copies start, and relinks if the other copy restarts. Both TX nodes also
play to the default sink, so you hear the exchange; add `--manual` to keep them silent.

## Transmitting through a radio

cw-chat sends CW as an audio tone, so the radio must be in a USB data mode (DATA-U on
Yaesu rigs) with the TX node's audio routed to the radio's data input: link
`cw-chat-NAME-tx` in qpwgraph, or pass `--target`. The radio turns the tone into CW
at the dial frequency plus the tone. **TX tone follows RX tone** (on by default, in
the settings menu) sends on the pitch RX is tuned to, which answers the other
station on their frequency. Keep the audio drive low enough that the radio's ALC
barely moves.

Without `--ptt`, keying is up to the radio (VOX, or PTT by hand). With `--ptt hrdctl`,
cw-chat keys the transmitter through Ham Radio Deluxe using the `hrdctl` command from
[smc-bridge-hrdctl](https://github.com/dhtseany/smc-bridge-hrdctl), which must be
installed and able to reach HRD's TCP server:

```sh
cw-chat --name FT710 --ptt hrdctl                         # hrdctl's default HRD host
cw-chat --ptt hrdctl --hrd-host 172.16.10.3 --hrd-port 7809 --ptt-button TX
```

For each transmission it runs `hrdctl button TX on`, waits `--ptt-lead-ms` (200) for
the radio to switch, plays the messages (keeping the transmitter keyed across
back-to-back messages), then runs `hrdctl unkey` `--ptt-tail-ms` (150) after the audio.
The header shows **KEYING** and then **ON AIR** while the radio is keyed, and the
settings menu has a **Key the radio (PTT)** switch.

- Stop (Esc), a failed key, closing the window, and a watchdog (keyed 10 s past the
  end of the expected audio) all stop the audio and unkey.
- A key whose outcome is unknown (hrdctl exit code 3, or no answer within 20 s) counts
  as keyed, so it is unkeyed too. If a key fails, nothing is sent.
- A failed unkey is retried every 2 s, and the badge reads "unkey failed, retrying"
  until it succeeds. Only a transmitter cw-chat keyed is unkeyed.
- These are software safeguards. Keep a way to unkey that does not depend on
  cw-chat (the radio's own controls, `hrdctl unkey`, an smc-bridge STOP key), and
  set the radio's transmit timeout timer.

## Other modes

```sh
cw-chat send "CQ CQ DE W8ABC"           # send one message and exit
cw-chat send --wav hello.wav "HELLO"    # export instead of playing
cw-chat decode recording.wav            # decode a WAV file (any rate, mixed to mono;
                                        # tone found automatically unless --tone)
cw-chat console --name A --rx-from B    # terminal chat: stdin lines out, RX printed
```

`send` is the original one-shot transmitter: node `morse-tx`, `--target`/`--manual`
routing, a message-duration-plus-120-second timeout, and exit after PipeWire drains.

## Encoding and timing

ASCII letters (case insensitive), digits, and punctuation
`. , ? / = + - @` are supported. Whitespace is normalized to word gaps.
Unsupported characters and empty input are rejected before sending.

`HELLO TEST` encodes as:

```text
.... . .-.. .-.. ---   - . ... -
```

Dot duration is 1200/WPM milliseconds. Dashes are three dots; gaps are one
dot between elements, three between characters, and seven between words.
Gaps are total durations, not additive. Chat messages end with a word gap so queued
messages stay separate. Cumulative sample rounding avoids timing drift at fractional
dot lengths. A 5 ms raised-cosine attack/release reduces keying clicks.
WPM is limited to 1–100, frequency to 20–20,000 Hz, and gain to 0–1.
Input is limited to 4096 bytes and rendered transmissions to ten minutes.

## Decoding

- **Tone detection:** a Hann-windowed single-bin DFT (Goertzel-style), 10 ms wide
  (about 100 Hz bandwidth), evaluated every 5 ms at the RX tone.
- **Keying:** keys on when the tone is 3× (9.5 dB) above a running mean of the noise
  floor, with hysteresis, a 10 ms debounce, and a glitch filter that drops marks
  shorter than about a third of a dot. The first 100 ms set the noise floor from their
  quietest quarter and are then replayed, so audio that starts mid-tone decodes.
  Tones longer than 2 s are treated as a carrier or new noise floor.
- **Auto tuning:** a bank of DFT bins every 10 Hz from 300 to 1200 Hz (21 ms window)
  finds tones that are well above the band's median and keyed (they return to the
  floor between elements, which excludes carriers). The decoder jumps only between
  characters: after a pause, or mid-over when nothing is keying at the current tone,
  replaying the last 0.25 s so a first letter is kept. Per-character pitch
  measurement then keeps it centred.
- **Stations:** pitch comes from the detector's phase rotation between hops (to
  within a few Hz); level is the mean keyed dBFS.
- **Timing:** dots and dashes are clustered separately and track the sender's speed
  (5–60 WPM) from any starting estimate; gaps are split at 2 and 5 units. Unknown
  patterns decode as `*`.

In tests, 20 WPM text decodes exactly at −6.5 dB SNR over 24 kHz of white noise
(40 of 40 trials), 95% at −9 dB, and about one false character appears per 400
seconds of pure noise. Uneven hand-sent timing (±25% per element) and 8–40 WPM
senders decode from a 20 WPM starting estimate.

## Structure

- `src/morse/`: character table (with reverse lookup) and strict text encoding
- `src/cw/timing.rs`: tone/silence events in dot units
- `src/cw/oscillator.rs`: sine generation, envelopes, sample timing
- `src/rx/`: tone detector, band-wide auto tuner, and adaptive decoder
- `src/audio/engine.rs`: long-lived PipeWire thread with TX and RX nodes, message
  queue, RX muting, and the `--rx-from` linker
- `src/audio/pipewire.rs`: one-shot playback for `send`
- `src/ptt.rs`: push-to-talk controller and the `hrdctl` keyer
- `src/ui/`: GTK4/libadwaita window
- `src/main.rs`: CLI, subcommands, WAV import/export

The engine runs its own PipeWire main loop on a separate thread; the window sends it
commands through a PipeWire channel and receives events through an async channel.
Stream callbacks copy pre-rendered samples and run on that loop (RT_PROCESS is not
enabled); there is no direct ALSA access.

## Integration checks

Both scripts start a private PipeWire server under a private config name, so no
system or user drop-ins (hardware, network, or custom sinks) are loaded, and they
never touch the desktop server or physical devices.

- `python scripts/pipewire-smoke.py` (after `cargo build`): runs `send --manual`
  without a session manager, links TX to a capture stream, and checks every captured
  HELLO TEST sample against WAV export. Requires `pipewire`, `pw-cat`, `pw-cli`,
  `pw-dump`, and `pw-link`.
- `scripts/two-copies-smoke.sh` (after `cargo build --release`): adds WirePlumber
  with hardware monitors disabled, runs console copies A and B linked with
  `--rx-from`, and checks that each decoded the other's over.

Validated in development: 36 unit tests, formatting, and Clippy pass; both scripts
pass; both GUI copies start, link, and stay up under a headless compositor. The
window has decoded live receive audio from a Yaesu FT-710; push-to-talk has been
tested end to end against a stand-in `hrdctl`, not yet on the air.

## License

Copyright (C) 2026 Sean Snell

This program is free software: you can redistribute it and/or modify it under the
terms of the GNU General Public License as published by the Free Software
Foundation, either version 3 of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See the GNU General Public License for more details.

See [LICENSE](LICENSE) for the full text.
