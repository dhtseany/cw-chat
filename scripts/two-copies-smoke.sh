#!/bin/bash
# Two-copy engine test: after `cargo build --release`, run scripts/two-copies-smoke.sh.
# Starts a private PipeWire + WirePlumber (no hardware or network modules, no desktop
# D-Bus), runs `cw-chat console` copies A and B linked to each other, has each send
# one over, and checks that the other decoded it.
set -u
BIN=$(cd "$(dirname "$0")/.." && pwd)/target/release/cw-chat
ROOT=$(mktemp -d /tmp/cw-chat-smoke.XXXX)   # short: socket paths are limited to 108 bytes
trap 'kill $WP $PW 2>/dev/null; wait 2>/dev/null; rm -rf "$ROOT"' EXIT
mkdir -p "$ROOT"/run "$ROOT"/config/wireplumber/wireplumber.conf.d "$ROOT"/state
cat > "$ROOT"/config/wireplumber/wireplumber.conf.d/99-isolated.conf <<'CONF'
wireplumber.profiles = {
  main = {
    monitor.alsa = disabled
    monitor.alsa-midi = disabled
    monitor.bluez = disabled
    monitor.bluez.midi = disabled
    monitor.v4l2 = disabled
    monitor.libcamera = disabled
  }
}
CONF
export XDG_RUNTIME_DIR=$ROOT/run PIPEWIRE_RUNTIME_DIR=$ROOT/run
export XDG_CONFIG_HOME=$ROOT/config XDG_STATE_HOME=$ROOT/state
unset DBUS_SESSION_BUS_ADDRESS PIPEWIRE_REMOTE
# A private config name means no pipewire.conf.d drop-ins (system or user) are loaded.
cp /usr/share/pipewire/pipewire.conf "$ROOT"/isolated.conf
pipewire -c "$ROOT"/isolated.conf > "$ROOT"/pipewire.log 2>&1 & PW=$!
for _ in $(seq 100); do [ -S "$ROOT"/run/pipewire-0 ] && break; sleep 0.05; done
wireplumber > "$ROOT"/wireplumber.log 2>&1 & WP=$!
sleep 1
pw-cli create-node adapter '{ factory.name=support.null-audio-sink node.name=test-sink
    media.class=Audio/Sink object.linger=true audio.position=[FL FR] }' > /dev/null
sleep 0.5

( sleep 9; echo "A DE B GM"; sleep 8 ) |
    "$BIN" console --name B --rx-from A > "$ROOT"/b.out 2> "$ROOT"/b.err &
B=$!
sleep 0.5
( sleep 0.5; echo "CQ CQ DE A K"; sleep 16 ) |
    "$BIN" console --name A --rx-from B > "$ROOT"/a.out 2> "$ROOT"/a.err &
A=$!
wait $A $B

for side in a b; do echo "== $side"; cat "$ROOT"/$side.out "$ROOT"/$side.err; done
if grep -q "^RX: CQ CQ DE A K" "$ROOT"/b.out && grep -q "^RX: A DE B GM" "$ROOT"/a.out; then
    echo "PASS: each copy decoded the other's over"
else
    echo "FAIL"; exit 1
fi
