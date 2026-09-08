#!/usr/bin/env python3
"""Fake mpv: serves the JSON IPC socket, reports a scripted time-pos, exits.
Env: FAKE_MPV_STOP_AT (secs, default 95),
     FAKE_MPV_RUNTIME (wall secs to stay alive, default 1.5),
     FAKE_MPV_NO_IPC (if set, never create the socket, simulating a wrapper that ignores
     --input-ipc-server or a share too slow to bind in time)."""
import json, os, socket, sys, threading, time

sock_path = next(a.split("=", 1)[1] for a in sys.argv if a.startswith("--input-ipc-server="))
duration = 100.0  # every test reports the same duration; only STOP_AT and RUNTIME vary
stop_at = float(os.environ.get("FAKE_MPV_STOP_AT", "95"))
runtime = float(os.environ.get("FAKE_MPV_RUNTIME", "1.5"))
start = time.time()

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
                val = duration if prop == "duration" else stop_at * frac
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
