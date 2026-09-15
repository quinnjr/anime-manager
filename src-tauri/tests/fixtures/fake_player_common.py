#!/usr/bin/env python3
"""Shared scaffolding for the fake player fixtures (fake_mpv.py, fake_vlc.py).

Only the parts identical in spirit across both fakes live here: env-var
helpers, argv capture, the NO_IPC sleep-exit, and the unix-socket accept loop.
Each fixture keeps its distinctive part: mpv the JSON get_property dispatch
with request_id matching, VLC the RC line-command reply map.
"""
import os
import socket
import sys
import threading
import time


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


def write_argv_out(argv, path_env):
    """Write the received argv (one per line) to the path named by path_env.

    No-op when the env var is unset; exits on write failure, mirroring the
    inline code this replaced.
    """
    argv_out = os.environ.get(path_env)
    if argv_out:
        try:
            with open(argv_out, "w") as f:
                f.write("\n".join(argv))
        except OSError as e:
            sys.exit(f"could not write {path_env} {argv_out!r}: {e}")


def exit_if_no_ipc(flag_env, runtime):
    """Sleep-exit when the NO_IPC flag is set, simulating a player that never
    creates its control socket. Returns otherwise."""
    if os.environ.get(flag_env):
        time.sleep(runtime)
        sys.exit(0)


def serve_unix(sock_path, runtime, handler):
    """Bind a unix socket and dispatch each connection to handler in a daemon
    thread, for runtime wall seconds. Unlinks a stale socket before binding."""
    if os.path.exists(sock_path):
        os.unlink(sock_path)
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(sock_path)
    srv.listen(1)
    srv.settimeout(0.2)
    deadline = time.time() + runtime
    while time.time() < deadline:
        try:
            conn, _ = srv.accept()
            threading.Thread(target=handler, args=(conn,), daemon=True).start()
        except socket.timeout:
            pass
