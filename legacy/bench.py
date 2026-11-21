#!/usr/bin/env python3
"""
bench.py — measure the PC half of the Eyepiece pipeline (legacy eyepiece_stream.py) without the goggles.

  encode   frame → encoded-AU latency per encoder/bitrate.  We write raw yuv420p
           frames into ffmpeg's stdin at the target fps and timestamp each one.
           CAVEAT (measured): ffmpeg's rawvideo/y4m pipe demuxers only release
           frame n once frame n+1 arrives, so this mode reads ONE FRAME (16.7 ms
           @60) high.  Use it to compare encoders/bitrates; for absolute numbers
           use the real capture path (eyepiece_stream --source x11 prints capture→send).
  python   cost of eyepiece_stream's FLV parse + socket send per frame, and the
           maximum frame rate it can sustain (no -re, as fast as possible).

  ./bench.py encode --encoder x264 --bitrate 12M
  ./bench.py encode --sweep            # table over encoders × bitrates × options
  ./bench.py python --bitrate 50M
"""
import argparse
import importlib.util
import os
import socket
import statistics
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("eyepiece_stream", os.path.join(HERE, "eyepiece_stream.py"))
vs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(vs)


def make_frames(size, n):
    """n distinct raw yuv420p frames of moving test content (from ffmpeg)."""
    w, h = map(int, size.split("x"))
    fsz = w * h * 3 // 2
    raw = subprocess.run(["ffmpeg", "-v", "error", "-f", "lavfi", "-i", f"testsrc2=size={size}:rate=60",
                          "-frames:v", str(n), "-pix_fmt", "yuv420p", "-f", "rawvideo", "-"],
                         capture_output=True, check=True).stdout
    return [raw[i * fsz:(i + 1) * fsz] for i in range(n)], fsz


def encoder_args(a):
    """Reuse eyepiece_stream's encoder settings, but with raw stdin as the input."""
    class A: pass
    o = A()
    o.fps, o.codec, o.encoder, o.bitrate = a.fps, a.codec, a.encoder, a.bitrate
    o.gop, o.slices, o.source, o.ffmpeg_extra = a.gop, a.slices, "custom", a.extra
    o.input_args = f"-f rawvideo -pix_fmt yuv420p -s {vs.PANEL_SIZE} -r {a.fps} -i pipe:0"
    o.region = o.display = o.input = o.kms_device = None
    o.vaapi_device = "/dev/dri/renderD128"
    o.intra_refresh = getattr(a, "intra_refresh", False)
    o.vbv_frames = getattr(a, "vbv_frames", 1.0)
    return vs.build_ffmpeg_cmd(o)


def bench_encode(a, frames, quiet=False):
    cmd = encoder_args(a)
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
    t_in = {}
    n = a.frames
    period = 1.0 / a.fps

    def feeder():
        t0 = time.perf_counter()
        try:
            for i in range(n):
                target = t0 + i * period
                while True:
                    d = target - time.perf_counter()
                    if d <= 0:
                        break
                    time.sleep(min(d, 0.002))
                t_in[i] = time.perf_counter()
                p.stdin.write(frames[i % len(frames)])
            p.stdin.close()
        except BrokenPipeError:
            pass

    th = threading.Thread(target=feeder, daemon=True)
    th.start()
    lat, sizes, keys = [], [], []
    i = 0
    try:
        for au, key, codec, ts in vs.FlvReader(p.stdout).frames():
            t = time.perf_counter()
            if i in t_in:
                lat.append((t - t_in[i]) * 1000)
            sizes.append(len(au))
            keys.append(key)
            i += 1
    except EOFError:
        pass
    p.wait()
    err = p.stderr.read().decode(errors="replace").strip()
    if not lat:
        return dict(error=err[-300:] or "no output")
    warm = lat[min(30, len(lat) // 4):]          # skip encoder warm-up
    ks = [s for s, k in zip(sizes, keys) if k] or [0]
    ps = [s for s, k in zip(sizes, keys) if not k] or [0]
    return dict(n=len(lat), p50=statistics.median(warm), p95=sorted(warm)[int(len(warm) * .95)],
                max=max(warm), key_kb=statistics.mean(ks) / 1024, p_kb=statistics.mean(ps) / 1024,
                p_max_kb=max(ps) / 1024, mbit=sum(sizes) * 8 / (len(sizes) / a.fps) / 1e6)


def bench_python(a):
    """FLV parse + send cost, flat out, into a socket drained by a thread."""
    srv = socket.socket()
    srv.bind(("127.0.0.1", 0))
    srv.listen(1)
    cli = socket.create_connection(srv.getsockname())
    cli.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    con, _ = srv.accept()
    threading.Thread(target=lambda: [None for _ in iter(lambda: con.recv(1 << 20), b"")], daemon=True).start()
    # encode once to a file so the encoder is not the bottleneck
    a2 = argparse.Namespace(**vars(a))
    a2.extra = f"-frames:v {a.frames}"
    cmd = [c for c in vs.build_ffmpeg_cmd(_flat_args(a2)) if c != "-re"]
    flv = subprocess.run(cmd, capture_output=True, check=True).stdout
    import io
    per = []
    t0 = time.perf_counter()
    nbytes = 0
    for au, key, codec, ts in vs.FlvReader(io.BytesIO(flv)).frames_safe():
        t = time.perf_counter()
        pkt = vs.struct.pack("<QB3x", vs.mono_us(), codec) + au
        cli.sendall(vs.HDR.pack(86, 82, 1, key, len(pkt)) + pkt)
        per.append((time.perf_counter() - t) * 1e6)
        nbytes += len(au)
    dt = time.perf_counter() - t0
    return dict(frames=len(per), fps_max=len(per) / dt, mbit_max=nbytes * 8 / dt / 1e6,
                send_us_p50=statistics.median(per), send_us_max=max(per),
                avg_kb=nbytes / len(per) / 1024)


def _flat_args(a):
    class A: pass
    o = A()
    for k, v in vars(a).items():
        setattr(o, k, v)
    o.source, o.ffmpeg_extra = "test", a.extra
    o.region = o.display = o.input = o.kms_device = None
    o.input_args = ""
    o.vaapi_device = "/dev/dri/renderD128"
    o.intra_refresh = getattr(a, "intra_refresh", False)
    o.vbv_frames = getattr(a, "vbv_frames", 1.0)
    return o


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("mode", choices=["encode", "python"])
    ap.add_argument("--fps", type=int, default=60)
    ap.add_argument("--codec", default="h264")
    ap.add_argument("--encoder", default="x264")
    ap.add_argument("--bitrate", default="12M")
    ap.add_argument("--gop", type=int, default=30)
    ap.add_argument("--slices", type=int, default=4)
    ap.add_argument("--frames", type=int, default=300)
    ap.add_argument("--intra-refresh", action="store_true")
    ap.add_argument("--vbv-frames", type=float, default=1.0)
    ap.add_argument("--extra", default="")
    ap.add_argument("--sweep", action="store_true")
    a = ap.parse_args()

    if a.mode == "python":
        print(bench_python(a))
        return

    frames, _ = make_frames(vs.PANEL_SIZE, 60)
    if not a.sweep:
        print(bench_encode(a, frames))
        return
    print(f"{'encoder':8} {'bitrate':>7} {'opts':14} {'p50ms':>6} {'p95ms':>6} {'maxms':>6} "
          f"{'IDR kB':>7} {'P kB':>6} {'Pmax kB':>7} {'Mbit/s':>6}")
    for enc in ("x264", "vaapi"):
        for br in ("6M", "12M", "25M", "50M"):
            for ir in (False, True):
                b = argparse.Namespace(**vars(a))
                b.encoder, b.bitrate, b.intra_refresh = enc, br, ir
                r = bench_encode(b, frames)
                opts = "intra-refresh" if ir else "IDR/gop"
                if "error" in r:
                    print(f"{enc:8} {br:>7} {opts:14} ERROR {r['error'][-120:]}")
                    continue
                print(f"{enc:8} {br:>7} {opts:14} {r['p50']:6.1f} {r['p95']:6.1f} {r['max']:6.1f} "
                      f"{r['key_kb']:7.1f} {r['p_kb']:6.1f} {r['p_max_kb']:7.1f} {r['mbit']:6.1f}", flush=True)


if __name__ == "__main__":
    main()
