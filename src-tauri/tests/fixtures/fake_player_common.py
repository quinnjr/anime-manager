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


def serve_tcp(addr, runtime, handler):
    """Like serve_unix, on a loopback TCP port given as host:port (VLC's RC
    transport on Windows, which has no --rc-unix)."""
    host, port = addr.rsplit(":", 1)
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.bind((host, int(port)))
    srv.listen(1)
    srv.settimeout(0.2)
    deadline = time.time() + runtime
    while time.time() < deadline:
        try:
            conn, _ = srv.accept()
            threading.Thread(target=handler, args=(conn,), daemon=True).start()
        except socket.timeout:
            pass


class _PipeConn:
    """Just enough of a socket (recv, sendall, context manager) over one
    connected Windows named-pipe instance for the fakes' handlers."""

    def __init__(self, k32, handle):
        self.k32 = k32
        self.h = handle

    def recv(self, n):
        import ctypes
        buf = ctypes.create_string_buffer(n)
        got = ctypes.c_ulong(0)
        if not self.k32.ReadFile(self.h, buf, n, ctypes.byref(got), None):
            return b""
        return buf.raw[: got.value]

    def sendall(self, data):
        import ctypes
        done = ctypes.c_ulong(0)
        self.k32.WriteFile(self.h, data, len(data), ctypes.byref(done), None)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.k32.CloseHandle(self.h)


def serve_pipe(name, runtime, handler):
    """Like serve_unix, on a Windows named pipe (mpv's IPC transport there).
    ConnectNamedPipe blocks with no timeout, so the accept loop runs on a
    daemon thread and the process exits once runtime has elapsed."""
    import ctypes
    from ctypes import wintypes

    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    k32.CreateNamedPipeW.restype = wintypes.HANDLE
    k32.CreateNamedPipeW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                                     wintypes.DWORD, wintypes.DWORD, wintypes.DWORD,
                                     wintypes.DWORD, ctypes.c_void_p]
    k32.ConnectNamedPipe.argtypes = [wintypes.HANDLE, ctypes.c_void_p]
    k32.ReadFile.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD,
                             ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p]
    k32.WriteFile.argtypes = [wintypes.HANDLE, ctypes.c_char_p, wintypes.DWORD,
                              ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p]
    k32.CloseHandle.argtypes = [wintypes.HANDLE]
    PIPE_ACCESS_DUPLEX, PIPE_UNLIMITED = 0x3, 255
    ERROR_PIPE_CONNECTED = 535
    INVALID = wintypes.HANDLE(-1).value

    def accept_loop():
        while True:
            h = k32.CreateNamedPipeW(name, PIPE_ACCESS_DUPLEX, 0, PIPE_UNLIMITED,
                                     65536, 65536, 0, None)
            if h == INVALID or h is None:
                return
            if not k32.ConnectNamedPipe(h, None) and ctypes.get_last_error() != ERROR_PIPE_CONNECTED:
                k32.CloseHandle(h)
                continue
            threading.Thread(target=handler, args=(_PipeConn(k32, h),), daemon=True).start()

    threading.Thread(target=accept_loop, daemon=True).start()
    time.sleep(runtime)
    sys.stdout.flush()
    os._exit(0)


def serve_ipc(addr, runtime, handler):
    """Serve on whichever transport addr names: a Windows named pipe, a
    host:port TCP address, or a unix socket path."""
    if addr.startswith("\\\\.\\pipe\\"):
        serve_pipe(addr, runtime, handler)
    elif os.name == "nt":
        serve_tcp(addr, runtime, handler)
    else:
        serve_unix(addr, runtime, handler)
