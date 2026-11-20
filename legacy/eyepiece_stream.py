#!/usr/bin/env python3
"""
eyepiece_stream.py — legacy PC side of Eyepiece (superseded by eyepiece-pc): stream the PC screen to DJI Goggles 3 over
USB and get head tracking back.

    ffmpeg (capture + low-latency H.264) ──FLV pipe──► eyepiece_stream ──adb forward──► eyepiece_bridge (goggles)
    opentrack / your app ◄──UDP── eyepiece_stream ◄── head pose (yaw/pitch/roll) ◄── eyepiece_bridge

Only the Python standard library, ffmpeg and adb are needed.

Examples
    ./eyepiece_stream.py                                # bridge started from the goggles' menu
    ./eyepiece_stream.py --deploy                       # push + start eyepiece_bridge, stream X11 :0.0
    ./eyepiece_stream.py --source test                    # moving test pattern (latency check)
    ./eyepiece_stream.py --encoder nvenc --fps 60 --bitrate 15M
    ./eyepiece_stream.py --no-video --print-pose          # head tracking only
    gst-launch-1.0 ... ! flvmux streamable=true ! fdsink | ./eyepiece_stream.py --source flv-stdin

Keys (stdin): r + Enter = recenter, q + Enter = quit.
"""
import argparse
import queue
import os
import select
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time

PROTO_VERSION = 1
EP_VIDEO, EP_PING, EP_CTRL, EP_STREAM = 0x01, 0x02, 0x03, 0x04
EP_POSE, EP_PONG, EP_LOG, EP_FRAME, EP_INFO = 0x81, 0x82, 0x83, 0x84, 0x85
EP_FLAG_KEY = 0x01
EP_CODEC_H264, EP_CODEC_HEVC = 0, 1
EP_OP_RECENTER, EP_OP_QUIT = 1, 2
HDR = struct.Struct("<BBBBI")
START = b"\x00\x00\x00\x01"

HERE = os.path.dirname(os.path.abspath(__file__))
DEVICE_DIR = "/data/local/tmp/eyepiece"                       # --deploy target
MENU_BRIDGE = "/vendor/natartos/apps/eyepiece/eyepiece_bridge"  # natartos Eyepiece mod
DEVICE_LOG = "/data/local/tmp/eyepiece.log"


def find_bridge():
    """--deploy without --bridge: eyepiece_bridge next to this script, else
    $XDG_DATA_HOME/eyepiece/eyepiece_bridge."""
    data = os.environ.get("XDG_DATA_HOME") or os.path.join(os.path.expanduser("~"), ".local", "share")
    tried = [os.path.join(HERE, "eyepiece_bridge"), os.path.join(data, "eyepiece", "eyepiece_bridge")]
    for t in tried:
        if os.path.isfile(t):
            return t
    raise SystemExit(f"--deploy: eyepiece_bridge not found (looked in: {', '.join(tried)}). Download "
                     "eyepiece_bridge from the natartos release or pass --bridge. Without --deploy, the "
                     "bridge started from the goggles' menu (natartos Eyepiece mod) is used.")


def mono_us():
    return time.monotonic_ns() // 1000


def log(*a):
    print("[eyepiece-stream]", *a, file=sys.stderr, flush=True)


# --------------------------------------------------------------------------- adb

class Adb:
    def __init__(self, exe, serial):
        self.base = [exe] + (["-s", serial] if serial else [])

    def run(self, *args, check=True, capture=True, timeout=30):
        r = subprocess.run(self.base + list(args), capture_output=capture, text=True, timeout=timeout)
        if check and r.returncode != 0:
            raise RuntimeError(f"adb {' '.join(args)} failed: {(r.stderr or r.stdout).strip()}")
        return (r.stdout or "").strip()

    def shell(self, cmd, check=True, timeout=30):
        return self.run("shell", cmd, check=check, timeout=timeout)


def deploy(adb, bridge_path, bridge_args):
    if not os.path.isfile(bridge_path):
        raise SystemExit(f"{bridge_path} not found — download eyepiece_bridge from the natartos release")
    adb.shell(f"mkdir -p {DEVICE_DIR}")
    adb.run("push", bridge_path, f"{DEVICE_DIR}/eyepiece_bridge")
    adb.shell(f"chmod 755 {DEVICE_DIR}/eyepiece_bridge")
    stop_remote(adb)
    # nohup so the bridge survives the adb shell; logs in /data/local/tmp/eyepiece.log.
    # -1: exit when this PC session ends. While the bridge runs, the overlay draws its
    # buffer over the whole UI, so a bridge left running = black goggles; on exit the
    # overlay hands the display back (dji_glasses restart).
    adb.shell(f"nohup {DEVICE_DIR}/eyepiece_bridge -1 {bridge_args} >/dev/null 2>&1 < /dev/null &")
    time.sleep(0.8)
    pid = adb.shell("pidof eyepiece_bridge", check=False)
    if not pid:
        out = adb.shell(f"tail -n 20 {DEVICE_LOG}", check=False)
        raise SystemExit("eyepiece_bridge did not start:\n" + out)
    log(f"eyepiece_bridge running on goggles (pid {pid})")
    if not overlay_loaded(adb):
        log("WARNING: natartos overlay not detected (hook_overlay.so is not mapped in dji_glasses) — "
            "if the goggles stay black, start natartos (`toolya natartos start`); its overlay mod puts "
            "the stream on the display")


def overlay_loaded(adb):
    """natartos overlay hook loaded into dji_glasses? Process maps first (reliable), then
    natartos.log (may be truncated). True only on positive evidence."""
    out = adb.shell("for p in $(pidof dji_glasses); do grep -q hook_overlay /proc/$p/maps 2>/dev/null && "
                    "{ echo maps; exit 0; }; done; "
                    "grep -q 'hook_overlay loaded' /data/local/tmp/natartos.log 2>/dev/null && echo log",
                    check=False)
    return bool(out.strip())


def stop_remote(adb):
    pid = adb.shell("pidof eyepiece_bridge", check=False)
    if pid:
        adb.shell(f"kill {pid}", check=False)
        time.sleep(2.5)            # bridge helper sends the MCU attitude stop on SIGTERM
    mcu_stop_safety(adb)


def mcu_stop_safety(adb):
    """Belt and braces for the MCU head-attitude stream: if no bridge is running, send the
    stop (eyepiece_bridge -Z). Covers the one case the bridge can't (bridge and its helper both
    SIGKILLed): the stream outlives them and logs a router error per packet. Harmless when
    the stream is already off (MCU ACKs 00)."""
    if adb.shell("pidof eyepiece_bridge", check=False):
        return
    out = adb.shell(f"for b in {DEVICE_DIR}/eyepiece_bridge {MENU_BRIDGE}; do "
                    "[ -x $b ] && { $b -Z 2>&1 | grep 'MCU attitude'; break; }; done", check=False).strip()
    if out.startswith("[") and "] " in out:
        out = out.split("] ", 1)[1]          # drop the bridge's own log prefix
    if out:
        log("goggles: " + out)


# --------------------------------------------------------------------------- ffmpeg

def parse_rate(s):
    """'12M' / '800k' / '12000000' → bits per second."""
    mul = {"k": 1000, "m": 1000_000}.get(s[-1].lower(), 1)
    return int(float(s[:-1] if mul > 1 else s) * mul)


class TeeToPlayer:
    """Passes the FLV byte stream through unchanged and copies it into an ffplay window
    (what the goggles receive, artifacts included). The copy runs in its own thread with
    a bounded queue: if ffplay falls behind, it stops getting data — never the goggles."""

    def __init__(self, f, pos):
        x, y = (pos.split(",") + ["0"])[:2]
        self.f = f
        self.q = queue.Queue(maxsize=512)
        self.p = subprocess.Popen(
            ["ffplay", "-hide_banner", "-loglevel", "error", "-fflags", "nobuffer", "-flags", "low_delay",
             "-framedrop", "-sync", "ext", "-probesize", "32", "-analyzeduration", "0",
             "-window_title", "eyepiece_stream preview (what the goggles get)",
             "-left", x, "-top", y, "-x", "1920", "-y", "1080", "-f", "flv", "-i", "-"],
            stdin=subprocess.PIPE, bufsize=0)
        threading.Thread(target=self._feed, daemon=True).start()

    def _feed(self):
        while True:
            b = self.q.get()
            try:
                self.p.stdin.write(b)
            except (BrokenPipeError, OSError):
                return

    def read(self, n):
        b = self.f.read(n)
        if b:
            try:
                self.q.put_nowait(b)
            except queue.Full:
                pass
        return b


def build_ffmpeg_cmd(a):
    w, h = a.size.split("x")
    g = [a.ffmpeg, "-hide_banner", "-loglevel", "warning", "-nostdin",
         "-fflags", "nobuffer", "-flags", "low_delay", "-probesize", "32", "-analyzeduration", "0"]
    if a.source == "x11":
        region = a.region or a.size
        rw, rh = region.split("+")[0].split("x")
        off = region.split("+", 1)[1].replace("+", ",") if "+" in region else "0,0"
        g += ["-f", "x11grab", "-draw_mouse", "1", "-framerate", str(a.fps),
              "-video_size", f"{rw}x{rh}", "-i", f"{a.display}+{off}"]
    elif a.source == "kms":
        g += ["-device", a.kms_device, "-f", "kmsgrab", "-framerate", str(a.fps)]
        if a.kms_plane:
            g += ["-plane_id", str(a.kms_plane)]
        g += ["-i", "-"]
    elif a.source == "test":
        g += ["-re", "-f", "lavfi", "-i", f"testsrc2=size={a.size}:rate={a.fps}"]
    elif a.source == "file":
        if not a.input:
            raise SystemExit("--source file needs --input FILE")
        g += ["-re", "-stream_loop", "-1", "-i", a.input]
    elif a.source == "custom":
        g += a.input_args.split()
    else:
        raise SystemExit(f"unknown source {a.source}")

    hevc = a.codec == "hevc"
    gop = str(a.gop or 2 * a.fps)        # default 2 s: frequent IDRs cost quality (TCP is lossless)
    br = a.bitrate
    # 1-frame VBV: the encoder never queues more than a frame of bits
    vbv_frames = getattr(a, "vbv_frames", 1.0)
    vbv = f"{max(1, int(parse_rate(br) * vbv_frames / a.fps / 1000))}k"
    ir = getattr(a, "intra_refresh", False)
    # kmsgrab hands out DRM-PRIME frames: map them into VAAPI and let the GPU convert/scale
    # (any framebuffer format the GPU reads), then encode on the GPU or download small NV12
    # bars are added while still RGB: pad_vaapi on NV12 writes Y=U=V=0 (green) whatever color=
    kms_gpu = (f"hwmap=derive_device=vaapi,scale_vaapi=w={w}:h={h}:mode=hq:force_original_aspect_ratio=decrease,"
               f"pad_vaapi=w={w}:h={h}:x=(ow-iw)/2:y=(oh-ih)/2:color=black,"
               f"scale_vaapi=format=nv12:out_color_matrix=bt709:out_range=tv")
    to_yuv = (f"scale={w}:{h}:flags=fast_bilinear:out_color_matrix=bt709:out_range=tv,format=yuv420p"
              if a.source != "kms" else kms_gpu + ",hwdownload,format=nv12")
    color = ["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv"]

    if a.encoder in ("x264", "auto"):          # software: libx264 / libx265
        enc = (["-c:v", "libx265", "-preset", "ultrafast", "-tune", "zerolatency",
                "-x265-params", f"keyint={gop}:min-keyint={gop}:scenecut=0:bframes=0:slices={a.slices}:repeat-headers=1"
                + (":intra-refresh=1" if ir else "")]
               if hevc else
               # ultrafast = CAVLC, no deblock, 1 ref: cheapest possible decode on the A55s
               ["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-profile:v", "baseline",
                "-x264-params", f"keyint={gop}:min-keyint={gop}:scenecut=0:slices={a.slices}:"
                                f"sliced-threads=1:repeat-headers=1:aud=0" + (":intra-refresh=1" if ir else "")])
        enc = ["-vf", to_yuv] + enc
    elif a.encoder == "nvenc":
        enc = ["-vf", to_yuv, "-c:v", "hevc_nvenc" if hevc else "h264_nvenc",
               "-preset", "p1", "-tune", "ull", "-zerolatency", "1", "-delay", "0", "-rc", "cbr",
               "-bf", "0", "-g", gop, "-slices", str(a.slices)]
        if ir:
            enc += ["-intra-refresh", "1"]
        if not hevc:   # the goggles decode in hardware: high/CABAC (+1.3 dB vs baseline/CAVLC)
            enc += ["-profile:v", "high", "-coder", "cabac"]
    elif a.encoder == "vaapi":
        if a.source == "kms":                 # zero-copy: capture → GPU scale → GPU encode
            pre = ["-vf", kms_gpu]
        else:
            pre = ["-vaapi_device", a.vaapi_device, "-vf",
                   to_yuv.replace("format=yuv420p", "format=nv12") + ",hwupload"]
        enc = pre + ["-c:v", "hevc_vaapi" if hevc else "h264_vaapi",
               "-bf", "0", "-g", gop, "-slices", str(a.slices), "-rc_mode", "CBR",
               "-async_depth", "1"]          # default 2 queues frames: +15 ms measured
        if not hevc:   # measured at 30M: baseline/CAVLC 45.2 dB → high/CABAC 46.5 dB PSNR
            enc += ["-profile:v", "high", "-coder", "cabac"]
    else:
        raise SystemExit(f"unknown encoder {a.encoder}")

    g += enc + color + ["-b:v", br, "-maxrate", br, "-bufsize", vbv, "-an"]
    if a.source in ("x11", "kms") and getattr(a, "capture_ts", False):
        # keep the grabbers' wall-clock capture time (shifted to fit FLV's 32-bit ms) so latency
        # is measured from capture. BROKEN for rate control: kmsgrab declares 1,000,000 fps, the
        # encoder then budgets bitrate/1e6 per frame (0.2–0.5 Mbit/s, heavy blocking; level 6.2).
        a.ts_base = int(time.time()) - 10
        g += ["-copyts", "-output_ts_offset", str(-a.ts_base), "-fps_mode", "passthrough"]
    else:
        # constant frame rate: the encoder knows the real fps → correct bit budget per frame
        a.ts_base = None
        g += ["-r", str(a.fps)]
    if a.ffmpeg_extra:
        g += a.ffmpeg_extra.split()
    g += ["-f", "flv", "-flvflags", "no_duration_filesize", "-flush_packets", "1", "pipe:1"]
    return g


# --------------------------------------------------------------------------- FLV → Annex-B

class FlvReader:
    """Minimal FLV demuxer: yields (annexb_access_unit, is_key, codec, flv_ts_ms)."""

    def __init__(self, f):
        self.f = f
        self.nal_len = 4
        self.param_sets = b""
        self.codec = EP_CODEC_H264

    def _read(self, n):
        b = bytearray()
        while len(b) < n:
            c = self.f.read(n - len(b))
            if not c:
                raise EOFError
            b += c
        return bytes(b)

    def _avcc(self, rec):
        self.nal_len = (rec[4] & 3) + 1
        out, p = [], 6
        for count_pos in ("sps", "pps"):
            n = rec[p - 1] & 0x1F if count_pos == "sps" else rec[p]
            if count_pos == "pps":
                p += 1
            for _ in range(n):
                ln = struct.unpack(">H", rec[p:p + 2])[0]
                out.append(START + rec[p + 2:p + 2 + ln])
                p += 2 + ln
        self.param_sets = b"".join(out)

    def _hvcc(self, rec):
        self.nal_len = (rec[21] & 3) + 1
        out, p = [], 23
        for _ in range(rec[22]):
            n = struct.unpack(">H", rec[p + 1:p + 3])[0]
            p += 3
            for _ in range(n):
                ln = struct.unpack(">H", rec[p:p + 2])[0]
                out.append(START + rec[p + 2:p + 2 + ln])
                p += 2 + ln
        self.param_sets = b"".join(out)

    def _nals(self, data):
        out, p, L = [], 0, self.nal_len
        while p + L <= len(data):
            ln = int.from_bytes(data[p:p + L], "big")
            p += L
            out.append(START + data[p:p + ln])
            p += ln
        return b"".join(out)

    def frames_safe(self):
        try:
            yield from self.frames()
        except EOFError:
            return

    def frames(self):
        hdr = self._read(9)
        if hdr[:3] != b"FLV":
            raise RuntimeError("encoder output is not FLV")
        self._read(hdr[8] - 9 + 4 if hdr[8] > 9 else 4)
        while True:
            th = self._read(11)
            ttype, size = th[0] & 0x1F, int.from_bytes(th[1:4], "big")
            ts = int.from_bytes(th[4:7], "big") | th[7] << 24
            body = self._read(size)
            self._read(4)
            if ttype != 9 or not body:
                continue
            b0 = body[0]
            if b0 & 0x80:                              # enhanced FLV (HEVC)
                ftype, ptype, fourcc = (b0 >> 4) & 7, b0 & 0x0F, body[1:5]
                if fourcc not in (b"hvc1", b"avc1"):
                    continue
                self.codec = EP_CODEC_HEVC if fourcc == b"hvc1" else EP_CODEC_H264
                if ptype == 0:
                    (self._hvcc if fourcc == b"hvc1" else self._avcc)(body[5:])
                    continue
                if ptype == 1:
                    payload = body[8:]
                elif ptype == 3:
                    payload = body[5:]
                else:
                    continue
            else:                                      # legacy AVC
                ftype, codec_id = b0 >> 4, b0 & 0x0F
                if codec_id != 7:
                    continue
                self.codec = EP_CODEC_H264
                if body[1] == 0:
                    self._avcc(body[5:])
                    continue
                if body[1] != 1:
                    continue
                payload = body[5:]
            key = ftype == 1
            au = self._nals(payload)
            if key:
                au = self.param_sets + au
            yield au, key, self.codec, ts


# --------------------------------------------------------------------------- link

class Link:
    def __init__(self, a):
        self.a = a
        self.sock = None
        self.tx_lock = threading.Lock()
        self.alive = True
        self.offset_us = None        # dev_clock - pc_clock
        self.best_rtt = 1 << 62
        self.ot = socket.socket(socket.AF_INET, socket.SOCK_DGRAM) if a.opentrack else None
        if self.ot:
            host, port = a.opentrack.rsplit(":", 1)
            self.ot_addr = (host, int(port))
        self.stats = dict(frames=0, lat=[], dec=[], cvt=[], q=0, dropped=0, poses=0, sent=0, bytes=0)
        self.last_pose = None
        self.cap_lat = []            # capture→send µs (x11/kms sources)
        self.stats_lock = threading.Lock()

    def connect(self, timeout=15):
        t_end = time.time() + timeout
        last = None
        while time.time() < t_end:
            try:
                s = socket.create_connection(("127.0.0.1", self.a.port), timeout=2)
                s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                s.settimeout(None)
                self.sock = s
                return
            except OSError as e:
                last = e
                time.sleep(0.5)
        raise SystemExit(f"cannot connect to eyepiece_bridge on 127.0.0.1:{self.a.port} ({last}); "
                         f"is it running and is `adb forward tcp:{self.a.port} tcp:{self.a.port}` set?")

    def send(self, mtype, payload, flags=0):
        with self.tx_lock:
            self.sock.sendall(HDR.pack(ord("E"), ord("P"), mtype, flags, len(payload)) + payload)

    def _recv_exact(self, n):
        b = bytearray()
        while len(b) < n:
            c = self.sock.recv(n - len(b))
            if not c:
                raise EOFError
            b += c
        return bytes(b)

    def rx_loop(self):
        try:
            while self.alive:
                m0, m1, mtype, flags, ln = HDR.unpack(self._recv_exact(HDR.size))
                if (m0, m1) != (ord("E"), ord("P")):
                    raise RuntimeError("bad magic from goggles")
                p = self._recv_exact(ln) if ln else b""
                self.handle(mtype, p)
        except (EOFError, OSError) as e:
            if self.alive:
                log(f"connection to goggles closed ({e})")
        self.alive = False

    def handle(self, mtype, p):
        if mtype == EP_POSE:
            t, yaw, pitch, roll, active, src, _ = struct.unpack("<QfffBBH", p[:24])
            self.last_pose = (yaw, pitch, roll, active, src)
            with self.stats_lock:
                self.stats["poses"] += 1
            if self.ot:
                # opentrack "UDP over network" input: 6 little-endian doubles x,y,z,yaw,pitch,roll
                self.ot.sendto(struct.pack("<6d", 0, 0, 0, yaw, pitch, roll), self.ot_addr)
            if self.a.print_pose:
                print(f"pose yaw={yaw:7.2f} pitch={pitch:7.2f} roll={roll:7.2f} src={src}", flush=True)
        elif mtype == EP_PONG:
            t_pc, t_dev = struct.unpack("<QQ", p[:16])
            now = mono_us()
            rtt = now - t_pc
            if rtt < self.best_rtt or self.offset_us is None:
                self.best_rtt = rtt
                self.offset_us = t_dev - (t_pc + rtt // 2)
        elif mtype == EP_FRAME:
            pts, t_recv, t_shown, dec, cvt, q, dropped = struct.unpack("<QQQIIII", p[:40])
            with self.stats_lock:
                s = self.stats
                s["frames"] += 1
                s["dec"].append(dec)
                s["cvt"].append(cvt)
                s["q"] = max(s["q"], q)
                s["dropped"] = dropped
                if self.offset_us is not None:
                    s["lat"].append(t_shown - self.offset_us - pts)
        elif mtype == EP_LOG:
            log("goggles:", p.decode("utf-8", "replace"))
        elif mtype == EP_INFO:
            ver, fw, fh, dec, ht, _ = struct.unpack("<IHHBBH", p[:12])
            log(f"bridge v{ver}: fb {fw}x{fh}, decoder={({1: 'software', 2: 'hardware'}).get(dec, 'none')}, "
                f"headtrack flags={ht:#x}")
            if ver != PROTO_VERSION:
                log(f"WARNING: protocol version mismatch (bridge {ver}, pc {PROTO_VERSION})")

    def ping_loop(self):
        while self.alive:
            try:
                self.send(EP_PING, struct.pack("<Q", mono_us()))
            except OSError:
                break
            time.sleep(0.5 if self.offset_us is None else 2.0)

    def stats_loop(self, interval=3.0):
        def pct(v, q):
            v = sorted(v)
            return v[min(len(v) - 1, int(len(v) * q))] / 1000 if v else float("nan")
        while self.alive:
            time.sleep(interval)
            with self.stats_lock:
                s, self.stats = self.stats, dict(frames=0, lat=[], dec=[], cvt=[], q=0,
                                                 dropped=self.stats["dropped"], poses=0, sent=0, bytes=0)
                s_sent, s_bytes = s["sent"], s["bytes"]
                cap, self.cap_lat = self.cap_lat, []
            msg = (f"sent {s_sent / interval:5.1f} fps {s_bytes * 8 / interval / 1e6:5.1f} Mbit/s | "
                   f"shown {s['frames'] / interval:5.1f} fps | "
                   f"{'capture' if getattr(self.a, 'ts_base', None) is not None else 'send'}→shown "
                   f"p50 {pct(s['lat'], .5):5.1f} ms p95 {pct(s['lat'], .95):5.1f} ms"
                   f"{f' (capture→send {pct(cap, .5):4.1f})' if cap else ''} | "
                   f"decode {pct(s['dec'], .5):4.1f} ms convert {pct(s['cvt'], .5):4.1f} ms | "
                   f"queue≤{s['q']} dropped {s['dropped']} | pose {s['poses'] / interval:5.1f} Hz | "
                   f"rtt {self.best_rtt / 1000:.1f} ms")
            if self.last_pose:
                msg += f" | yaw {self.last_pose[0]:6.1f} pitch {self.last_pose[1]:6.1f}"
            log(msg)


def stdin_loop(link):
    while link.alive:
        r, _, _ = select.select([sys.stdin], [], [], 0.5)
        if not r:
            continue
        line = sys.stdin.readline()
        if not line:
            return
        c = line.strip().lower()
        if c == "r":
            link.send(EP_CTRL, bytes([EP_OP_RECENTER]))
            log("recenter sent")
        elif c == "q":
            link.alive = False


# --------------------------------------------------------------------------- main

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--adb", default=shutil.which("adb") or "adb")
    ap.add_argument("--serial", help="adb device serial")
    ap.add_argument("--port", type=int, default=42420)
    ap.add_argument("--no-forward", action="store_true", help="don't run adb forward (e.g. RNDIS/Wi-Fi)")
    ap.add_argument("--deploy", action="store_true", help="push eyepiece_bridge to the goggles and start it "
                         "(default: use the bridge started from the goggles' menu)")
    ap.add_argument("--bridge", default=None,
                    help="eyepiece_bridge binary for --deploy (default: next to this script, "
                         "else $XDG_DATA_HOME/eyepiece/eyepiece_bridge)")
    ap.add_argument("--bridge-args", default="", help="extra eyepiece_bridge args for --deploy (e.g. '-D hw', '-f')")
    ap.add_argument("--stop-bridge", action="store_true", help="ask the bridge to exit when we quit")
    ap.add_argument("--source", default="x11", choices=["x11", "kms", "test", "file", "custom", "flv-stdin"])
    ap.add_argument("--display", default=os.environ.get("DISPLAY", ":0.0"))
    ap.add_argument("--region", help="x11 capture region WxH+X+Y (default: --size at 0,0)")
    ap.add_argument("--kms-device", default="/dev/dri/card0")
    ap.add_argument("--kms-plane", type=int, default=0,
                    help="DRM plane to capture (one per monitor; 0 = first with a framebuffer). "
                         "Needs an 8-bit framebuffer: KDE uses 10-bit unless KWIN_DRM_PREFER_COLOR_DEPTH=24")
    ap.add_argument("--ffmpeg", default=os.environ.get("EYEPIECE_FFMPEG", "ffmpeg"),
                    help="ffmpeg binary (kms needs one with CAP_SYS_ADMIN, e.g. ~/.local/bin/ffmpeg-kms; env EYEPIECE_FFMPEG)")
    ap.add_argument("--input", help="file for --source file")
    ap.add_argument("--input-args", default="", help="ffmpeg input args for --source custom")
    ap.add_argument("--size", default="1920x1080", help="encoded size (goggles panel is 1920x1080)")
    ap.add_argument("--fps", type=int, default=60)
    ap.add_argument("--codec", default="h264", choices=["h264", "hevc"])
    ap.add_argument("--encoder", default="auto", choices=["auto", "x264", "nvenc", "vaapi"])
    ap.add_argument("--vaapi-device", default="/dev/dri/renderD128")
    ap.add_argument("--bitrate", default="12M")
    ap.add_argument("--gop", type=int, default=0, help="keyframe interval in frames (default 2 s)")
    ap.add_argument("--slices", type=int, default=4, help="slices per frame (= decoder threads on the goggles)")
    ap.add_argument("--intra-refresh", action="store_true",
                    help="gradual intra refresh instead of periodic IDR (no keyframe bursts; x264/x265/nvenc)")
    ap.add_argument("--vbv-frames", type=float, default=4.0,
                    help="encoder VBV size in frames of bitrate (default 4: keyframes get room; USB moves 4 frames in ~1 ms)")
    ap.add_argument("--ffmpeg-extra", default="", help="extra ffmpeg output args")
    ap.add_argument("--no-video", action="store_true", help="head tracking only")
    ap.add_argument("--opentrack", default="127.0.0.1:4242",
                    help="send pose to opentrack 'UDP over network' input (''=off)")
    ap.add_argument("--print-pose", action="store_true")
    ap.add_argument("--print-cmd", action="store_true", help="print the ffmpeg command and exit")
    ap.add_argument("--capture-ts", action="store_true",
                    help="x11/kms: carry capture timestamps for capture→shown stats (starves the encoder's "
                         "rate control with kmsgrab — diagnostics only)")
    ap.add_argument("--preview", nargs="?", const="0,160", default=None, metavar="X,Y",
                    help="also show the exact encoded stream in an ffplay window at screen position X,Y "
                         "(default 0,160)")
    a = ap.parse_args()

    if a.print_cmd:
        print(" ".join(build_ffmpeg_cmd(a)))
        return

    adb = Adb(a.adb, a.serial)
    if a.deploy:
        deploy(adb, a.bridge or find_bridge(), a.bridge_args)
    if not a.no_forward:
        adb.run("forward", f"tcp:{a.port}", f"tcp:{a.port}")

    link = Link(a)
    link.connect()
    log(f"connected to eyepiece_bridge via 127.0.0.1:{a.port}")
    for fn in (link.rx_loop, link.ping_loop, link.stats_loop):
        threading.Thread(target=fn, daemon=True).start()
    if sys.stdin.isatty():
        threading.Thread(target=stdin_loop, args=(link,), daemon=True).start()

    ff = None
    src = None
    try:
        if a.no_video:
            while link.alive:
                time.sleep(0.2)
            return
        w, h = map(int, a.size.split("x"))
        link.send(EP_STREAM, struct.pack("<HHBBH", w, h, EP_CODEC_HEVC if a.codec == "hevc" else EP_CODEC_H264,
                                         min(a.fps, 255), 0))
        if a.source == "flv-stdin":
            src = sys.stdin.buffer
        else:
            cmd = build_ffmpeg_cmd(a)
            log("encoder:", " ".join(cmd))
            ff = subprocess.Popen(cmd, stdout=subprocess.PIPE, bufsize=0)
            src = ff.stdout
        if a.preview:
            src = TeeToPlayer(src, a.preview)
        for au, key, codec, ts in FlvReader(src).frames():
            if not link.alive:
                break
            pts = mono_us()
            if a.ts_base is not None:      # capture wall time → our monotonic clock
                age_us = int((time.time() - a.ts_base - ts / 1000.0) * 1e6)
                if 0 <= age_us < 2_000_000:
                    pts -= age_us
                    link.cap_lat.append(age_us)
            pkt = struct.pack("<QB3x", pts, codec) + au
            link.send(EP_VIDEO, pkt, EP_FLAG_KEY if key else 0)
            with link.stats_lock:
                link.stats["sent"] += 1
                link.stats["bytes"] += len(au)
    except EOFError:
        log("encoder stream ended")
    except (KeyboardInterrupt, BrokenPipeError, ConnectionError):
        pass
    finally:
        link.alive = False
        if a.stop_bridge:
            try:
                link.send(EP_CTRL, bytes([EP_OP_QUIT]))
            except OSError:
                pass
        if a.deploy:              # the -1 bridge exits with the session; make sure the
            time.sleep(3)         # MCU attitude stream is off afterwards
            mcu_stop_safety(adb)
        if isinstance(src, TeeToPlayer):
            src.p.kill()          # ffplay ignores SIGTERM while its SDL window is open
        if ff:
            ff.terminate()
            try:
                ff.wait(2)
            except subprocess.TimeoutExpired:
                ff.kill()


if __name__ == "__main__":
    main()
