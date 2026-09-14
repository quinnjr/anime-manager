#!/usr/bin/env python3
"""Fake mpv: serves the JSON IPC socket, reports a scripted time-pos, exits.
Env: FAKE_MPV_STOP_AT (secs, default 95),
     FAKE_MPV_RUNTIME (wall secs to stay alive, default 1.5),
     FAKE_MPV_NO_IPC (if set, never create the socket, simulating a wrapper that ignores
     --input-ipc-server or a share too slow to bind in time),
     FAKE_MPV_VOLUME / FAKE_MPV_OSD_W / FAKE_MPV_OSD_H / FAKE_MPV_MINIMIZED /
     FAKE_MPV_MAXIMIZED / FAKE_MPV_FULLSCREEN
     (what the player state properties report),
     FAKE_MPV_ARGV_OUT (if set, write the received argv there so a test can assert the flags)."""
import json, os, socket, sys, threading, time

sock_path = next(a.split("=", 1)[1] for a in sys.argv if a.startswith("--input-ipc-server="))
duration = 100.0  # every test reports the same duration; only STOP_AT and RUNTIME vary
stop_at = float(os.environ.get("FAKE_MPV_STOP_AT", "95"))
runtime = float(os.environ.get("FAKE_MPV_RUNTIME", "1.5"))
start = time.time()

def env_float(name, default):
    raw = os.environ.get(name)
    if raw is None:
        return default
    try:
        return float(raw)
    except ValueError:
        sys.exit(f"{name} must be a number, got {raw!r}")

def env_int(name, default):
    raw = os.environ.get(name)
    if raw is None:
        return default
    try:
        return int(raw)
    except ValueError:
        sys.exit(f"{name} must be an integer, got {raw!r}")

def env_flag(name):
    raw = os.environ.get(name)
    if raw is None or raw == "":
        return False
    val = raw.lower()
    if val in ("1", "true", "yes", "on"):
        return True
    if val in ("0", "false", "no", "off"):
        return False
    sys.exit(f"{name} must be a boolean, got {raw!r}")

volume = env_float("FAKE_MPV_VOLUME", 100.0)
osd_w = env_int("FAKE_MPV_OSD_W", 1280)
osd_h = env_int("FAKE_MPV_OSD_H", 720)
minimized = env_flag("FAKE_MPV_MINIMIZED")
maximized = env_flag("FAKE_MPV_MAXIMIZED")
fullscreen = env_flag("FAKE_MPV_FULLSCREEN")

argv_out = os.environ.get("FAKE_MPV_ARGV_OUT")
if argv_out:
    try:
        with open(argv_out, "w") as f:
            f.write("\n".join(sys.argv[1:]))
    except OSError as e:
        sys.exit(f"could not write FAKE_MPV_ARGV_OUT {argv_out!r}: {e}")

def prop_value(prop, frac):
    if prop == "duration":
        return duration
    if prop == "volume":
        return volume
    if prop == "osd-width":
        return osd_w
    if prop == "osd-height":
        return osd_h
    if prop == "window-minimized":
        return minimized
    if prop == "window-maximized":
        return maximized
    if prop == "fullscreen":
        return fullscreen
    return stop_at * frac

def serve(conn):
    buf = b""
    with conn:
        while True:
            data = conn.recv(4096)
            if not data:
                return
            buf += data
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                req = json.loads(line)
                prop = req["command"][1]
                frac = min(1.0, (time.time() - start) / runtime)
                val = prop_value(prop, frac)
                conn.sendall((json.dumps({"request_id": req.get("request_id", 0), "error": "success", "data": val}) + "\n").encode())

if os.environ.get("FAKE_MPV_NO_IPC"):
    time.sleep(runtime)
    sys.exit(0)

if os.path.exists(sock_path):
    os.unlink(sock_path)
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(sock_path)
srv.listen(1)
srv.settimeout(0.2)
deadline = start + runtime
while time.time() < deadline:
    try:
        conn, _ = srv.accept()
        threading.Thread(target=serve, args=(conn,), daemon=True).start()
    except socket.timeout:
        pass
sys.exit(0)
