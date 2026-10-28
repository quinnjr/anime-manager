#!/usr/bin/env python3
"""Fake MPC-HC (Windows only): speaks the /slave window-message API, exits.
Mirrors fake_mpv.py: duration is always 100s; only STOP_AT and RUNTIME vary.
Opens a message-only window, sends CMD_CONNECT with its handle and
CMD_NOWPLAYING to the /slave window, then answers CMD_GETCURRENTPOSITION and
CMD_GETNOWPLAYING until RUNTIME has passed.
Env: FAKE_MPC_STOP_AT (secs, default 95),
     FAKE_MPC_RUNTIME (wall secs to stay alive, default 1.5),
     FAKE_MPC_NO_IPC (if set, never connect, simulating a build or wrapper
     that ignores /slave),
     FAKE_MPC_STRANGER (if set, follow every position reply with a forged one
     near the end, claiming to come from another window, which must be ignored),
     FAKE_MPC_ARGV_OUT (if set, write the received argv there)."""
import ctypes
import os
import sys
import time
from ctypes import wintypes

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fake_player_common import env_flag, env_float, exit_if_no_ipc, write_argv_out

CMD_CONNECT = 0x50000000
CMD_NOWPLAYING = 0x50000003
CMD_CURRENTPOSITION = 0x50000007
CMD_DISCONNECT = 0x5000000B
CMD_GETNOWPLAYING = 0xA0003002
CMD_GETCURRENTPOSITION = 0xA0003004
WM_COPYDATA = 0x004A
HWND_MESSAGE = wintypes.HWND(-3)
PM_REMOVE = 0x0001
QS_ALLINPUT = 0x04FF

args = sys.argv[1:]
write_argv_out(args, "FAKE_MPC_ARGV_OUT")
master = None
for i, a in enumerate(args):
    if a.lower() == "/slave" and i + 1 < len(args):
        master = int(args[i + 1])
if master is None:
    sys.exit("/slave is required")

duration = 100.0  # every test reports the same duration; only STOP_AT and RUNTIME vary
stop_at = env_float("FAKE_MPC_STOP_AT", 95)
runtime = env_float("FAKE_MPC_RUNTIME", 1.5)
stranger = env_flag("FAKE_MPC_STRANGER")
exit_if_no_ipc("FAKE_MPC_NO_IPC", runtime)

LRESULT = ctypes.c_ssize_t
WNDPROC = ctypes.WINFUNCTYPE(LRESULT, wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM)


class COPYDATASTRUCT(ctypes.Structure):
    _fields_ = [("dwData", ctypes.c_size_t), ("cbData", wintypes.DWORD), ("lpData", ctypes.c_void_p)]


class WNDCLASSW(ctypes.Structure):
    _fields_ = [("style", wintypes.UINT), ("lpfnWndProc", WNDPROC), ("cbClsExtra", ctypes.c_int),
                ("cbWndExtra", ctypes.c_int), ("hInstance", wintypes.HINSTANCE), ("hIcon", wintypes.HICON),
                ("hCursor", wintypes.HANDLE), ("hbrBackground", wintypes.HBRUSH),
                ("lpszMenuName", wintypes.LPCWSTR), ("lpszClassName", wintypes.LPCWSTR)]


user32 = ctypes.WinDLL("user32", use_last_error=True)
user32.DefWindowProcW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
user32.DefWindowProcW.restype = LRESULT
user32.RegisterClassW.argtypes = [ctypes.POINTER(WNDCLASSW)]
user32.RegisterClassW.restype = wintypes.ATOM
user32.CreateWindowExW.argtypes = [wintypes.DWORD, wintypes.LPCWSTR, wintypes.LPCWSTR, wintypes.DWORD,
                                   ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int,
                                   wintypes.HWND, wintypes.HMENU, wintypes.HINSTANCE, ctypes.c_void_p]
user32.CreateWindowExW.restype = wintypes.HWND
user32.SendMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
user32.SendMessageW.restype = LRESULT
user32.PeekMessageW.argtypes = [ctypes.POINTER(wintypes.MSG), wintypes.HWND, wintypes.UINT,
                                wintypes.UINT, wintypes.UINT]
user32.TranslateMessage.argtypes = [ctypes.POINTER(wintypes.MSG)]
user32.DispatchMessageW.argtypes = [ctypes.POINTER(wintypes.MSG)]
user32.MsgWaitForMultipleObjects.argtypes = [wintypes.DWORD, ctypes.c_void_p, wintypes.BOOL,
                                             wintypes.DWORD, wintypes.DWORD]

start = time.time()
me = None


def send(cmd, text, sender=None):
    buf = ctypes.create_unicode_buffer(text)
    cds = COPYDATASTRUCT(cmd, ctypes.sizeof(buf), ctypes.cast(buf, ctypes.c_void_p))
    user32.SendMessageW(master, WM_COPYDATA, sender if sender is not None else me, ctypes.addressof(cds))


def now_playing():
    # A piped description exercises the \| escape MPC-HC applies inside fields.
    send(CMD_NOWPLAYING, "Fake|nobody|a \\| piped description|C:\\fake.mkv|%.3f" % duration)


@WNDPROC
def proc(hwnd, msg, wparam, lparam):
    if msg == WM_COPYDATA:
        cds = COPYDATASTRUCT.from_address(lparam)
        if cds.dwData == CMD_GETCURRENTPOSITION:
            frac = min(1.0, (time.time() - start) / runtime)
            send(CMD_CURRENTPOSITION, "%.3f" % (stop_at * frac))
            if stranger:
                send(CMD_CURRENTPOSITION, "%.3f" % (duration - 1), sender=1)
        elif cds.dwData == CMD_GETNOWPLAYING:
            now_playing()
        return 1
    return user32.DefWindowProcW(hwnd, msg, wparam, lparam)


wc = WNDCLASSW()
wc.lpfnWndProc = proc
wc.lpszClassName = "FakeMpcHc"
if not user32.RegisterClassW(ctypes.byref(wc)):
    sys.exit("RegisterClassW failed: %d" % ctypes.get_last_error())
me = user32.CreateWindowExW(0, "FakeMpcHc", None, 0, 0, 0, 0, 0, HWND_MESSAGE, None, None, None)
if not me:
    sys.exit("CreateWindowExW failed: %d" % ctypes.get_last_error())

send(CMD_CONNECT, str(me))
now_playing()

msg = wintypes.MSG()
while time.time() - start < runtime:
    user32.MsgWaitForMultipleObjects(0, None, False, 20, QS_ALLINPUT)
    while user32.PeekMessageW(ctypes.byref(msg), None, 0, 0, PM_REMOVE):
        user32.TranslateMessage(ctypes.byref(msg))
        user32.DispatchMessageW(ctypes.byref(msg))

# What a real MPC-HC sends as /close ends it: no CMD_NOTIFYENDOFSTREAM.
send(CMD_DISCONNECT, "")
