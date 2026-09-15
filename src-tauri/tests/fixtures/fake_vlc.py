#!/usr/bin/env python3
"""Fake VLC: serves the RC interface over a unix socket, exits.
Mirrors fake_mpv.py: duration is always 100s; only STOP_AT and RUNTIME vary.
Env: FAKE_VLC_STOP_AT (secs, default 95),
     FAKE_VLC_RUNTIME (wall secs to stay alive, default 1.5),
     FAKE_VLC_NO_IPC (if set, never create the socket, simulating a build
     without rc-unix or a share too slow to bind in time),
     FAKE_VLC_VOLUME (0-512 steps, default 256) reported for `volume`,
     FAKE_VLC_FULLSCREEN (bool) reported for `fullscreen`,
      FAKE_VLC_ARGV_OUT (if set, write the received argv there so a test can
      assert the flags),
      FAKE_VLC_GARBAGE_TIME (if set, reply this verbatim to `get_time`),
      FAKE_VLC_GARBAGE_VOLUME (if set, reply this verbatim to `volume`),
      FAKE_VLC_FULLSCREEN_RAW (if set, reply this verbatim to `fullscreen`)."""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fake_player_common import (
    env_flag,
    env_int,
    exit_if_no_ipc,
    serve_unix,
    write_argv_out,
)

args = sys.argv[1:]
sock_path = None
for i, a in enumerate(args):
    if a == "--rc-unix" and i + 1 < len(args):
        sock_path = args[i + 1]
    elif a.startswith("--rc-unix="):
        sock_path = a.split("=", 1)[1]
if sock_path is None:
    sys.exit("--rc-unix is required (fake VLC speaks RC over a unix socket)")

duration = 100.0  # every test reports the same duration; only STOP_AT and RUNTIME vary
stop_at = float(os.environ.get("FAKE_VLC_STOP_AT", "95"))
runtime = float(os.environ.get("FAKE_VLC_RUNTIME", "1.5"))
start = time.time()


volume = env_int("FAKE_VLC_VOLUME", 256)
fullscreen = env_flag("FAKE_VLC_FULLSCREEN")
garbage_time = os.environ.get("FAKE_VLC_GARBAGE_TIME")
garbage_volume = os.environ.get("FAKE_VLC_GARBAGE_VOLUME")
fullscreen_raw = os.environ.get("FAKE_VLC_FULLSCREEN_RAW")

write_argv_out(args, "FAKE_VLC_ARGV_OUT")


# Per-command reply map. Opt-in env hooks (e.g. garbage-injection knobs) slot
# in here as early branches on their command, above the plain reply.
def reply_for(cmd, frac):
    if cmd == "get_time":
        if garbage_time is not None:
            return garbage_time
        return str(stop_at * frac)
    if cmd == "get_length":
        return str(duration)
    if cmd == "volume":
        if garbage_volume is not None:
            return garbage_volume
        return str(volume)
    if cmd == "fullscreen":
        if fullscreen_raw is not None:
            return fullscreen_raw
        return "on" if fullscreen else "off"
    return "ok"


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
                cmd = line.decode(errors="replace").strip().split()
                frac = min(1.0, (time.time() - start) / runtime)
                resp = reply_for(cmd[0] if cmd else "", frac)
                conn.sendall((resp + "\n").encode())


exit_if_no_ipc("FAKE_VLC_NO_IPC", runtime)
serve_unix(sock_path, runtime, serve)
