#!/usr/bin/env python3
"""
vrstream.py — PC side of usb-vr: stream the PC screen to DJI Goggles 3 over
USB and get head tracking back.

    ffmpeg (capture + low-latency H.264) ──FLV pipe──► vrstream ──adb forward──► vr_bridge (goggles)
    opentrack / your app ◄──UDP── vrstream ◄── head pose (yaw/pitch) ◄────────── vr_bridge

Only the Python standard library, ffmpeg and adb are needed.

Examples
    ./vrstream.py --deploy                         # push + start vr_bridge, stream X11 :0.0
    ./vrstream.py --source test                    # moving test pattern (latency check)
    ./vrstream.py --encoder nvenc --fps 60 --bitrate 15M
    ./vrstream.py --no-video --print-pose          # head tracking only
    gst-launch-1.0 ... ! flvmux streamable=true ! fdsink | ./vrstream.py --source flv-stdin

Keys (stdin): r + Enter = recenter, q + Enter = quit.
"""
import argparse
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
VR_VIDEO, VR_PING, VR_CTRL, VR_STREAM = 0x01, 0x02, 0x03, 0x04
VR_POSE, VR_PONG, VR_LOG, VR_FRAME, VR_INFO = 0x81, 0x82, 0x83, 0x84, 0x85
VR_FLAG_KEY = 0x01
VR_CODEC_H264, VR_CODEC_HEVC = 0, 1
VR_OP_RECENTER, VR_OP_QUIT = 1, 2
HDR = struct.Struct("<BBBBI")
START = b"\x00\x00\x00\x01"

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_BRIDGE = os.path.join(HERE, "..", "out", "vr_bridge")
DEVICE_DIR = "/data/local/tmp/usbvr"


def mono_us():
    return time.monotonic_ns() // 1000


def log(*a):
    print("[vrstream]", *a, file=sys.stderr, flush=True)


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
        raise SystemExit(f"{bridge_path} not found — build it with usb-vr/device/build.sh")
    adb.shell(f"mkdir -p {DEVICE_DIR}")
    adb.run("push", bridge_path, f"{DEVICE_DIR}/vr_bridge")
    adb.shell(f"chmod 755 {DEVICE_DIR}/vr_bridge")
    stop_remote(adb)
    # nohup so the bridge survives the adb shell; logs in /data/local/tmp/usbvr.log
    adb.shell(f"nohup {DEVICE_DIR}/vr_bridge {bridge_args} >/dev/null 2>&1 < /dev/null &")
    time.sleep(0.8)
    pid = adb.shell("pidof vr_bridge", check=False)
    if not pid:
        out = adb.shell("tail -n 20 /data/local/tmp/usbvr.log", check=False)
        raise SystemExit("vr_bridge did not start:\n" + out)
    log(f"vr_bridge running on goggles (pid {pid})")
    nat = adb.shell("grep -c 'hook_overlay loaded' /data/local/tmp/natartos.log", check=False)
    if nat.strip() in ("", "0"):
        log("WARNING: natartos overlay not seen in natartos.log — video needs `toolya natartos start` "
            "(overlay mod injects fb_shared into the display)")


def stop_remote(adb):
    pid = adb.shell("pidof vr_bridge", check=False)
    if pid:
        adb.shell(f"kill {pid}", check=False)
        time.sleep(0.5)


# --------------------------------------------------------------------------- ffmpeg

def parse_rate(s):
    """'12M' / '800k' / '12000000' → bits per second."""
    mul = {"k": 1000, "m": 1000_000}.get(s[-1].lower(), 1)
    return int(float(s[:-1] if mul > 1 else s) * mul)


def build_ffmpeg_cmd(a):
    w, h = a.size.split("x")
    g = ["ffmpeg", "-hide_banner", "-loglevel", "warning", "-nostdin",
         "-fflags", "nobuffer", "-flags", "low_delay", "-probesize", "32", "-analyzeduration", "0"]
    if a.source == "x11":
        region = a.region or a.size
        rw, rh = region.split("+")[0].split("x")
        off = region.split("+", 1)[1].replace("+", ",") if "+" in region else "0,0"
        g += ["-f", "x11grab", "-draw_mouse", "1", "-framerate", str(a.fps),
              "-video_size", f"{rw}x{rh}", "-i", f"{a.display}+{off}"]
    elif a.source == "kms":
        g += ["-device", a.kms_device, "-f", "kmsgrab", "-framerate", str(a.fps), "-i", "-"]
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
    gop = str(a.gop)
    br = a.bitrate
    # 1-frame VBV: the encoder never queues more than a frame of bits
    vbv_frames = getattr(a, "vbv_frames", 1.0)
    vbv = f"{max(1, int(parse_rate(br) * vbv_frames / a.fps / 1000))}k"
    ir = getattr(a, "intra_refresh", False)
    to_yuv = (f"scale={w}:{h}:flags=fast_bilinear:out_color_matrix=bt709:out_range=tv,format=yuv420p"
              if a.source != "kms" else
              f"hwdownload,format=bgr0,scale={w}:{h}:flags=fast_bilinear:out_color_matrix=bt709:out_range=tv,format=yuv420p")
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
        if not hevc:
            enc += ["-profile:v", "baseline", "-coder", "cavlc"]
    elif a.encoder == "vaapi":
        up = to_yuv.replace("format=yuv420p", "format=nv12") + ",hwupload"
        enc = ["-vaapi_device", a.vaapi_device, "-vf", up, "-c:v", "hevc_vaapi" if hevc else "h264_vaapi",
               "-bf", "0", "-g", gop, "-slices", str(a.slices), "-rc_mode", "CBR",
               "-async_depth", "1"]          # default 2 queues frames: +15 ms measured
        if not hevc:
            enc += ["-profile:v", "constrained_baseline", "-coder", "cavlc"]
    else:
        raise SystemExit(f"unknown encoder {a.encoder}")

    g += enc + color + ["-b:v", br, "-maxrate", br, "-bufsize", vbv, "-an"]
    if a.source in ("x11", "kms"):
        # x11grab/kmsgrab stamp frames with wall-clock capture time: keep it
        # (shifted to fit FLV's 32-bit ms) so latency is measured from capture
        a.ts_base = int(time.time()) - 10
        g += ["-copyts", "-output_ts_offset", str(-a.ts_base), "-fps_mode", "passthrough"]
    else:
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
        self.codec = VR_CODEC_H264

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
                self.codec = VR_CODEC_HEVC if fourcc == b"hvc1" else VR_CODEC_H264
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
                self.codec = VR_CODEC_H264
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
        raise SystemExit(f"cannot connect to vr_bridge on 127.0.0.1:{self.a.port} ({last}); "
                         f"is it running and is `adb forward tcp:{self.a.port} tcp:{self.a.port}` set?")

    def send(self, mtype, payload, flags=0):
        with self.tx_lock:
            self.sock.sendall(HDR.pack(ord("V"), ord("R"), mtype, flags, len(payload)) + payload)

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
                if (m0, m1) != (ord("V"), ord("R")):
                    raise RuntimeError("bad magic from goggles")
                p = self._recv_exact(ln) if ln else b""
                self.handle(mtype, p)
        except (EOFError, OSError) as e:
            if self.alive:
                log(f"connection to goggles closed ({e})")
        self.alive = False

    def handle(self, mtype, p):
        if mtype == VR_POSE:
            t, yaw, pitch, roll, active, src, _ = struct.unpack("<QfffBBH", p[:24])
            self.last_pose = (yaw, pitch, roll, active, src)
            with self.stats_lock:
                self.stats["poses"] += 1
            if self.ot:
                # opentrack "UDP over network" input: 6 little-endian doubles x,y,z,yaw,pitch,roll
                self.ot.sendto(struct.pack("<6d", 0, 0, 0, yaw, pitch, roll), self.ot_addr)
            if self.a.print_pose:
                print(f"pose yaw={yaw:7.2f} pitch={pitch:7.2f} active={active} src={'bus' if src == 1 else 'hook'}",
                      flush=True)
        elif mtype == VR_PONG:
            t_pc, t_dev = struct.unpack("<QQ", p[:16])
            now = mono_us()
            rtt = now - t_pc
            if rtt < self.best_rtt or self.offset_us is None:
                self.best_rtt = rtt
                self.offset_us = t_dev - (t_pc + rtt // 2)
        elif mtype == VR_FRAME:
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
        elif mtype == VR_LOG:
            log("goggles:", p.decode("utf-8", "replace"))
        elif mtype == VR_INFO:
            ver, fw, fh, dec, ht, _ = struct.unpack("<IHHBBH", p[:12])
            log(f"bridge v{ver}: fb {fw}x{fh}, decoder={({1: 'software', 2: 'hardware'}).get(dec, 'none')}, "
                f"headtrack flags={ht:#x}")
            if ver != PROTO_VERSION:
                log(f"WARNING: protocol version mismatch (bridge {ver}, pc {PROTO_VERSION})")

    def ping_loop(self):
        while self.alive:
            try:
                self.send(VR_PING, struct.pack("<Q", mono_us()))
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
                   f"{'capture' if self.a.source in ('x11', 'kms') else 'send'}→shown "
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
            link.send(VR_CTRL, bytes([VR_OP_RECENTER]))
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
    ap.add_argument("--deploy", action="store_true", help="push out/vr_bridge to the goggles and start it")
    ap.add_argument("--bridge", default=DEFAULT_BRIDGE, help="vr_bridge binary for --deploy")
    ap.add_argument("--bridge-args", default="", help="extra vr_bridge args for --deploy (e.g. '-D hw', '-f')")
    ap.add_argument("--stop-bridge", action="store_true", help="ask the bridge to exit when we quit")
    ap.add_argument("--source", default="x11", choices=["x11", "kms", "test", "file", "custom", "flv-stdin"])
    ap.add_argument("--display", default=os.environ.get("DISPLAY", ":0.0"))
    ap.add_argument("--region", help="x11 capture region WxH+X+Y (default: --size at 0,0)")
    ap.add_argument("--kms-device", default="/dev/dri/card0")
    ap.add_argument("--input", help="file for --source file")
    ap.add_argument("--input-args", default="", help="ffmpeg input args for --source custom")
    ap.add_argument("--size", default="1920x1080", help="encoded size (goggles panel is 1920x1080)")
    ap.add_argument("--fps", type=int, default=60)
    ap.add_argument("--codec", default="h264", choices=["h264", "hevc"])
    ap.add_argument("--encoder", default="auto", choices=["auto", "x264", "nvenc", "vaapi"])
    ap.add_argument("--vaapi-device", default="/dev/dri/renderD128")
    ap.add_argument("--bitrate", default="12M")
    ap.add_argument("--gop", type=int, default=30, help="keyframe interval (frames) — also the loss-recovery time")
    ap.add_argument("--slices", type=int, default=4, help="slices per frame (= decoder threads on the goggles)")
    ap.add_argument("--intra-refresh", action="store_true",
                    help="gradual intra refresh instead of periodic IDR (no keyframe bursts; x264/x265/nvenc)")
    ap.add_argument("--vbv-frames", type=float, default=1.0,
                    help="encoder VBV size in frames of bitrate (1 = no frame ever exceeds 1/fps of airtime)")
    ap.add_argument("--ffmpeg-extra", default="", help="extra ffmpeg output args")
    ap.add_argument("--no-video", action="store_true", help="head tracking only")
    ap.add_argument("--opentrack", default="127.0.0.1:4242",
                    help="send pose to opentrack 'UDP over network' input (''=off)")
    ap.add_argument("--print-pose", action="store_true")
    ap.add_argument("--print-cmd", action="store_true", help="print the ffmpeg command and exit")
    a = ap.parse_args()

    if a.print_cmd:
        print(" ".join(build_ffmpeg_cmd(a)))
        return

    adb = Adb(a.adb, a.serial)
    if a.deploy:
        deploy(adb, a.bridge, a.bridge_args)
    if not a.no_forward:
        adb.run("forward", f"tcp:{a.port}", f"tcp:{a.port}")

    link = Link(a)
    link.connect()
    log(f"connected to vr_bridge via 127.0.0.1:{a.port}")
    for fn in (link.rx_loop, link.ping_loop, link.stats_loop):
        threading.Thread(target=fn, daemon=True).start()
    if sys.stdin.isatty():
        threading.Thread(target=stdin_loop, args=(link,), daemon=True).start()

    ff = None
    try:
        if a.no_video:
            while link.alive:
                time.sleep(0.2)
            return
        w, h = map(int, a.size.split("x"))
        link.send(VR_STREAM, struct.pack("<HHBBH", w, h, VR_CODEC_HEVC if a.codec == "hevc" else VR_CODEC_H264,
                                         min(a.fps, 255), 0))
        if a.source == "flv-stdin":
            src = sys.stdin.buffer
        else:
            cmd = build_ffmpeg_cmd(a)
            log("encoder:", " ".join(cmd))
            ff = subprocess.Popen(cmd, stdout=subprocess.PIPE, bufsize=0)
            src = ff.stdout
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
            link.send(VR_VIDEO, pkt, VR_FLAG_KEY if key else 0)
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
                link.send(VR_CTRL, bytes([VR_OP_QUIT]))
            except OSError:
                pass
        if ff:
            ff.terminate()
            try:
                ff.wait(2)
            except subprocess.TimeoutExpired:
                ff.kill()


if __name__ == "__main__":
    main()
