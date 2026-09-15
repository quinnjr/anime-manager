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
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fake_player_common import (
    env_flag,
    env_float,
    env_int,
    exit_if_no_ipc,
    serve_unix,
    write_argv_out,
)

sock_path = next(a.split("=", 1)[1] for a in sys.argv if a.startswith("--input-ipc-server="))
duration = 100.0  # every test reports the same duration; only STOP_AT and RUNTIME vary
stop_at = float(os.environ.get("FAKE_MPV_STOP_AT", "95"))
runtime = float(os.environ.get("FAKE_MPV_RUNTIME", "1.5"))
start = time.time()

volume = env_float("FAKE_MPV_VOLUME", 100.0)
osd_w = env_int("FAKE_MPV_OSD_W", 1280)
osd_h = env_int("FAKE_MPV_OSD_H", 720)
minimized = env_flag("FAKE_MPV_MINIMIZED")
maximized = env_flag("FAKE_MPV_MAXIMIZED")
fullscreen = env_flag("FAKE_MPV_FULLSCREEN")

write_argv_out(sys.argv[1:], "FAKE_MPV_ARGV_OUT")

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

exit_if_no_ipc("FAKE_MPV_NO_IPC", runtime)
serve_unix(sock_path, runtime, serve)
