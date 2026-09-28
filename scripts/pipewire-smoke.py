"""Optional native integration test: cargo build, then python scripts/pipewire-smoke.py."""
import os, subprocess, tempfile, time, json, pathlib, struct
ROOT = pathlib.Path(__file__).resolve().parents[1]
with tempfile.TemporaryDirectory(prefix="cw-chat-pw-") as runtime:
    env = dict(os.environ, XDG_RUNTIME_DIR=runtime, PIPEWIRE_RUNTIME_DIR=runtime)
    children = []
    try:
        log = open(runtime + "/daemon.log", "w")
        # A private config name skips pipewire.conf.d drop-ins, so network or
        # hardware modules from system or user configuration are not loaded.
        config = runtime + "/cw-isolated.conf"
        subprocess.run(["cp", "/usr/share/pipewire/pipewire.conf", config], check=True)
        daemon = subprocess.Popen(["pipewire", "-c", config], env=env, stdout=log, stderr=log)
        children.append(daemon)
        for _ in range(100):
            if os.path.exists(runtime + "/pipewire-0"): break
            time.sleep(.05)
        capture = subprocess.Popen(["pw-cat", "--record", "--target", "0", "--rate", "48000", "--channels", "1", "--format", "f32", runtime + "/captured.wav"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        children.append(capture)
        tx = subprocess.Popen([str(ROOT / "target/debug/cw-chat"), "send", "--manual", "HELLO TEST"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        children.append(tx)
        time.sleep(.5)
        graph = json.loads(subprocess.run(["pw-dump"], env=env, capture_output=True, text=True).stdout)
        for obj in graph:
            if obj["type"] != "PipeWire:Interface:Node": continue
            name = obj["info"]["props"].get("node.name", "")
            if name not in ("morse-tx", "pw-cat"): continue
            direction = "Output" if name == "morse-tx" else "Input"
            param = '{ direction: ' + direction + ', mode: dsp, format: { mediaType: audio, mediaSubtype: raw, format: F32P, rate: 48000, channels: 1, position: [ MONO ] } }'
            subprocess.run(["pw-cli", "set-param", str(obj["id"]), "PortConfig", param], env=env, check=True, stdout=subprocess.DEVNULL)
        for _ in range(100):
            output = subprocess.run(["pw-link", "-o"], env=env, capture_output=True, text=True).stdout
            inputs = subprocess.run(["pw-link", "-i"], env=env, capture_output=True, text=True).stdout
            sources = [p.strip() for p in output.splitlines() if "morse-tx:" in p]
            sinks = [p.strip() for p in inputs.splitlines() if "pw-cat:" in p]
            if sources and sinks: break
            time.sleep(.05)
        else:
            print(open(runtime + "/daemon.log").read())
            print("TX", tx.poll(), "CAP", capture.poll())
            if tx.poll() is not None: print(tx.communicate())
            if capture.poll() is not None: print(capture.communicate())
            raise RuntimeError(f"Ports missing: {output!r}, {inputs!r}")
        subprocess.run(["pw-link", sources[0], sinks[0]], env=env, check=True)
        stdout, stderr = tx.communicate(timeout=20)
        print("TX exit:", tx.returncode, stdout.decode(), stderr.decode())
        assert tx.returncode == 0
        capture.terminate()
        capture.wait(timeout=5)
        expected_path = runtime + "/expected.wav"
        subprocess.run([str(ROOT / "target/debug/cw-chat"), "send", "--wav", expected_path, "HELLO TEST"], check=True, stdout=subprocess.DEVNULL)
        def pcm(path):
            raw = pathlib.Path(path).read_bytes()
            offset = 12
            while offset + 8 <= len(raw):
                tag = raw[offset:offset + 4]
                size = struct.unpack_from("<I", raw, offset + 4)[0]
                if tag == b"data": return raw[offset + 8:offset + 8 + size]
                offset += 8 + size + size % 2
            raise AssertionError("Missing WAV data")
        expected = pcm(expected_path)
        actual = pcm(runtime + "/captured.wav")
        assert len(expected) == 221760 * 4
        assert expected in actual, "PipeWire capture differs from rendered samples"
        print("PASS: all 221760 transmitted frames match exactly")
    finally:
        for child in reversed(children):
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=5)
