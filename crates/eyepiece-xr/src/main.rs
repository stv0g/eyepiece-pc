//! eyepiece-xr — feed the goggles' head pose into Monado (OpenXR runtime, Linux).
//!
//! Monado ships a `remote` driver: monado-service listens on TCP 4242 and takes
//! `struct r_remote_data` packets (head pose, per-view FOV/pose, two controllers) from a
//! client. eyepiece-xr is that client. It reads the pose that eyepiece-pc already sends
//! to opentrack (UDP, 6 little-endian f64: x, y, z [cm], yaw, pitch, roll [deg]) and
//! forwards it, together with the goggles' optics (FOV, IPD), to Monado.
//!
//! Nothing here links against Monado or OpenXR: Monado is only started when VR is used
//! (`P_OVERRIDE_ACTIVE_CONFIG=remote monado-service`), eyepiece-pc stays unchanged.
//!
//! Wire layout follows Monado 25.0 `src/xrt/drivers/remote/r_interface.h` (BSL-1.0);
//! the packet is 376 bytes, header "mndrmt3\0". The hub sends two packets (reset, latest)
//! after accept; they are read and checked against the expected size and header.

use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::time::{Duration, Instant};

const HEADER: &[u8; 8] = b"mndrmt3\0";
const PACKET: usize = 376;
const OFF_HEAD: usize = 8; // r_head_data: views[2] (48 B each), center (28 B), valid + pad
const OFF_CENTER: usize = OFF_HEAD + 96;
const OFF_VALID: usize = OFF_CENTER + 28;
const OFF_LEFT: usize = OFF_HEAD + 128; // r_remote_controller_data, 120 B each
const OFF_RIGHT: usize = OFF_LEFT + 120;
const CTRL_ACTIVE: usize = 104 + 1; // after hand_tracking_active

struct Opts {
    monado: String,
    listen: String,
    fov_h: f64,
    fov_v: f64,
    ipd: f64,
    height: f64,
    test: bool,
    verbose: bool,
    driver_fov: bool,
    predict_ms: f64,
}

fn usage() -> ! {
    eprintln!(
        "usage: eyepiece-xr [--monado HOST:PORT] [--listen ADDR:PORT] [--fov-diag DEG | --fov HxV]\n\
         \x20                  [--ipd MM] [--height M] [--test] [-v]\n\
         \n\
         Forwards eyepiece-pc's opentrack pose (UDP) to Monado's remote driver.\n\
         \x20 --monado   monado-service remote driver (default 127.0.0.1:4242, TCP)\n\
         \x20 --listen   UDP pose input (default 127.0.0.1:4243; run eyepiece-pc --opentrack 127.0.0.1:4243)\n\
         \x20 --fov-diag diagonal FOV of one eye, 16:9 (default 44 = DJI Goggles 3 spec)\n\
         \x20 --fov      horizontal x vertical FOV in degrees, e.g. 38.8x22.4 (overrides --fov-diag)\n\
         \x20 --ipd      eye distance in mm (default 63)\n\
         \x20 --height   eye height in m (default 1.6)\n\
         \x20 --test     no UDP input: slow synthetic yaw/pitch sweep (for checking the setup)\n\
         \x20 --driver-fov  don't send FOV/eye poses; use the driver's fixed 85° views (fills the window)\n\
         \x20 --predict MS  extrapolate the pose MS milliseconds ahead from its angular rate (default 0)"
    );
    std::process::exit(2)
}

fn parse() -> Opts {
    let mut o = Opts {
        monado: "127.0.0.1:4242".into(),
        listen: "127.0.0.1:4243".into(),
        fov_h: 0.0,
        fov_v: 0.0,
        ipd: 63.0,
        height: 1.6,
        test: false,
        verbose: false,
        driver_fov: false,
        predict_ms: 0.0,
    };
    let mut diag = 44.0f64;
    let mut a = std::env::args().skip(1);
    while let Some(k) = a.next() {
        let mut v = || a.next().unwrap_or_else(|| usage());
        match k.as_str() {
            "--monado" => o.monado = v(),
            "--listen" => o.listen = v(),
            "--fov-diag" => diag = v().parse().unwrap_or_else(|_| usage()),
            "--fov" => {
                let s = v();
                let (h, w) = s.split_once('x').unwrap_or_else(|| usage());
                o.fov_h = h.parse().unwrap_or_else(|_| usage());
                o.fov_v = w.parse().unwrap_or_else(|_| usage());
            }
            "--ipd" => o.ipd = v().parse().unwrap_or_else(|_| usage()),
            "--height" => o.height = v().parse().unwrap_or_else(|_| usage()),
            "--test" => o.test = true,
            "--driver-fov" => o.driver_fov = true,
            "--predict" => o.predict_ms = v().parse().unwrap_or_else(|_| usage()),
            "-v" | "--verbose" => o.verbose = true,
            _ => usage(),
        }
    }
    if o.fov_h <= 0.0 || o.fov_v <= 0.0 {
        // split a 16:9 diagonal FOV into horizontal/vertical (rectilinear)
        let t = (diag.to_radians() / 2.0).tan();
        let d = (16.0f64 * 16.0 + 9.0 * 9.0).sqrt();
        o.fov_h = 2.0 * (t * 16.0 / d).atan().to_degrees();
        o.fov_v = 2.0 * (t * 9.0 / d).atan().to_degrees();
    }
    o
}

type Quat = [f64; 4]; // x, y, z, w

fn qmul(a: Quat, b: Quat) -> Quat {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}
fn axis(x: f64, y: f64, z: f64, deg: f64) -> Quat {
    let h = deg.to_radians() / 2.0;
    [x * h.sin(), y * h.sin(), z * h.sin(), h.cos()]
}
/// Goggles convention (yaw + right, pitch + up, roll + right ear down, degrees) →
/// OpenXR (X right, Y up, -Z forward): yaw about -Y, pitch about +X, roll about -Z.
fn orientation(yaw: f64, pitch: f64, roll: f64) -> Quat {
    qmul(qmul(axis(0.0, 1.0, 0.0, -yaw), axis(1.0, 0.0, 0.0, pitch)), axis(0.0, 0.0, 1.0, -roll))
}

fn put_f32(b: &mut [u8], off: usize, v: f64) {
    b[off..off + 4].copy_from_slice(&(v as f32).to_le_bytes());
}
fn put_pose(b: &mut [u8], off: usize, q: Quat, p: [f64; 3]) {
    for (i, v) in q.iter().chain(p.iter()).enumerate() {
        put_f32(b, off + i * 4, *v);
    }
}

fn packet(o: &Opts, q: Quat) -> [u8; PACKET] {
    let mut b = [0u8; PACKET];
    b[..8].copy_from_slice(HEADER);
    let (h, v) = ((o.fov_h / 2.0).to_radians(), (o.fov_v / 2.0).to_radians());
    for (i, sign) in [(0usize, -1.0f64), (1, 1.0)] {
        let off = OFF_HEAD + i * 48;
        put_f32(&mut b, off, -h); // angle_left
        put_f32(&mut b, off + 4, h); // angle_right
        put_f32(&mut b, off + 8, v); // angle_up
        put_f32(&mut b, off + 12, -v); // angle_down
        // view pose relative to the head centre: half the IPD to each side, parallel axes
        put_pose(&mut b, off + 16, [0.0, 0.0, 0.0, 1.0], [sign * o.ipd / 2000.0, 0.0, 0.0]);
    }
    put_pose(&mut b, OFF_CENTER, q, [0.0, o.height, 0.0]);
    // per_view_data_valid: the app renders with our FOV. Monado 25's remote HMD still
    // composites into its fixed 85° display FOV, so our narrower views end up small in
    // the middle of each eye; --driver-fov sends no view data (app renders 85°, fills the
    // window, but the scale is wrong for the goggles' 44° optics).
    b[OFF_VALID] = !o.driver_fov as u8;
    // controllers: identity orientation, inactive (no hands; apps see no controllers)
    for off in [OFF_LEFT, OFF_RIGHT] {
        put_f32(&mut b, off + 12, 1.0);
        b[off + CTRL_ACTIVE] = 0;
    }
    b
}

fn connect(o: &Opts) -> TcpStream {
    loop {
        match TcpStream::connect(&o.monado) {
            Ok(mut s) => {
                let _ = s.set_nodelay(true);
                // the hub sends its reset + latest state first
                let mut first = [0u8; PACKET];
                let ok = (0..2).all(|_| s.read_exact(&mut first).is_ok() && &first[..8] == HEADER);
                if ok {
                    eprintln!("eyepiece-xr: connected to Monado remote driver at {}", o.monado);
                    return s;
                }
                eprintln!("eyepiece-xr: unexpected data from {} (Monado version mismatch?)", o.monado);
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("eyepiece-xr: {} ({e}); is `P_OVERRIDE_ACTIVE_CONFIG=remote monado-service` running? retrying", o.monado);
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn main() {
    let o = parse();
    eprintln!(
        "eyepiece-xr: FOV {:.1}° x {:.1}° per eye, IPD {:.1} mm, pose from {}",
        o.fov_h,
        o.fov_v,
        o.ipd,
        if o.test { "synthetic sweep".into() } else { format!("UDP {}", o.listen) }
    );
    let udp = if o.test {
        None
    } else {
        let u = UdpSocket::bind(&o.listen).unwrap_or_else(|e| {
            eprintln!("eyepiece-xr: bind {}: {e}", o.listen);
            std::process::exit(1)
        });
        u.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        Some(u)
    };
    let mut tcp = connect(&o);
    let t0 = Instant::now();
    let (mut n, mut last_log) = (0u64, Instant::now());
    let mut last = (0.0, 0.0, 0.0);
    let mut last_t = Instant::now();
    loop {
        let (yaw, pitch, roll) = match &udp {
            None => {
                std::thread::sleep(Duration::from_millis(10));
                let t = t0.elapsed().as_secs_f64();
                (30.0 * (t * 0.5).sin(), 10.0 * (t * 0.3).sin(), 0.0)
            }
            Some(u) => {
                let mut b = [0u8; 64];
                match u.recv(&mut b) {
                    Ok(48) => {
                        let f = |i: usize| f64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
                        (f(3), f(4), f(5))
                    }
                    Ok(_) => continue,
                    Err(_) => last, // no pose: keep the last one (Monado needs nothing new)
                }
            }
        };
        // Monado's remote HMD ignores the requested display time, so prediction (if any)
        // happens here: linear extrapolation of the Euler angles from the last two samples.
        let now = Instant::now();
        let (py, pp, pr) = if o.predict_ms > 0.0 {
            let dt = now.duration_since(last_t).as_secs_f64().max(1e-3);
            let k = (o.predict_ms / 1000.0 / dt).min(10.0);
            let wrap = |d: f64| (d + 540.0) % 360.0 - 180.0;
            (
                yaw + k * wrap(yaw - last.0),
                pitch + k * (pitch - last.1),
                roll + k * wrap(roll - last.2),
            )
        } else {
            (yaw, pitch, roll)
        };
        last = (yaw, pitch, roll);
        last_t = now;
        let p = packet(&o, orientation(py, pp, pr));
        if let Err(e) = tcp.write_all(&p) {
            eprintln!("eyepiece-xr: Monado connection lost ({e}), reconnecting");
            tcp = connect(&o);
        }
        n += 1;
        if o.verbose && last_log.elapsed() >= Duration::from_secs(1) {
            eprintln!("eyepiece-xr: {n} poses, yaw {yaw:6.1} pitch {pitch:6.1} roll {roll:6.1}");
            last_log = Instant::now();
        }
    }
}
